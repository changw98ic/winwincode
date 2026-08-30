// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use winwincode_audit::AuditScope;
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, ProjectId, RepositoryId, WorkspaceId,
};
use winwincode_integration::{
    ConnectorCallError, ConnectorCallErrorKind, ConnectorProtocol, ConnectorRegistration,
    ConnectorState, EnterpriseIntegrationId, InboundStatus, IntegrationErrorKind,
    IntegrationFramework, IntegrationLeaseId, IntegrationOperationKey, IntegrationStorage,
    JIRA_CONNECTOR_PROTOCOL, JiraClock, JiraConnectorConfig, JiraCredentialError,
    JiraCredentialErrorKind, JiraCredentialPort, JiraEnterpriseConnector, JiraEventMapperPort,
    JiraInboundEvent, JiraOAuthAccessToken, JiraOAuthScope, JiraProjectKey, JiraResourceKind,
    JiraSiteId, JiraTlsRoots, JiraWebhookHeaders, JiraWebhookRequestFactory, JiraWebhookSecret,
    JiraWebhookVerifier, NormalizedInboundEvent, OutboundAttemptResult, OutboundOperationState,
    OutboundRequest, RetryPolicy,
};

const WEBHOOK_SECRET: &[u8] = b"jira-webhook-secret-fixture";
const OAUTH_TOKEN: &str = "jira-oauth-token-fixture";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-jira-connector-{name}-{}-{sequence}",
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

fn site_id() -> JiraSiteId {
    JiraSiteId::try_new("site-fixture-1").expect("site id")
}

fn project_key() -> JiraProjectKey {
    JiraProjectKey::try_new("WWC").expect("project key")
}

fn base_config(endpoint: String, roots: JiraTlsRoots) -> JiraConnectorConfig {
    JiraConnectorConfig::try_new(
        integration_id(),
        credential_reference_id(),
        site_id(),
        project_key(),
        endpoint,
        roots,
    )
    .expect("Jira connector config")
}

fn register(framework: &mut IntegrationFramework, config: &JiraConnectorConfig) {
    framework
        .register_connector(
            &ConnectorRegistration::try_new(
                config.integration_id().clone(),
                tenant_scope(),
                ConnectorProtocol::try_new(JIRA_CONNECTOR_PROTOCOL).expect("protocol"),
                config.credential_reference_id().clone(),
                10,
            )
            .expect("registration"),
        )
        .expect("register Jira connector");
}

#[derive(Clone, Copy)]
struct FixedClock(u64);

impl JiraClock for FixedClock {
    fn now_millis(&self) -> u64 {
        self.0
    }
}

#[derive(Clone)]
struct CredentialFixture {
    scopes: Vec<JiraOAuthScope>,
    revoked: bool,
    webhook_lookups: Arc<AtomicU64>,
    token_lookups: Arc<AtomicU64>,
}

impl CredentialFixture {
    fn full() -> Self {
        Self {
            scopes: vec![
                JiraOAuthScope::ReadIssue,
                JiraOAuthScope::WriteIssue,
                JiraOAuthScope::ReadComment,
                JiraOAuthScope::WriteComment,
            ],
            revoked: false,
            webhook_lookups: Arc::new(AtomicU64::new(0)),
            token_lookups: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl JiraCredentialPort for CredentialFixture {
    fn resolve_webhook_secret(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<JiraWebhookSecret, JiraCredentialError> {
        assert_eq!(reference, &credential_reference_id());
        self.webhook_lookups.fetch_add(1, Ordering::Relaxed);
        if self.revoked {
            Err(JiraCredentialError::new(JiraCredentialErrorKind::Revoked))
        } else {
            Ok(JiraWebhookSecret::try_new(WEBHOOK_SECRET).expect("webhook secret"))
        }
    }

    fn resolve_oauth_token(
        &mut self,
        reference: &CredentialReferenceId,
        requested_site: &JiraSiteId,
    ) -> Result<JiraOAuthAccessToken, JiraCredentialError> {
        assert_eq!(reference, &credential_reference_id());
        assert_eq!(requested_site, &site_id());
        self.token_lookups.fetch_add(1, Ordering::Relaxed);
        if self.revoked {
            return Err(JiraCredentialError::new(JiraCredentialErrorKind::Revoked));
        }
        JiraOAuthAccessToken::try_new(
            OAUTH_TOKEN,
            site_id(),
            [project_key()],
            self.scopes.clone(),
            100_000,
        )
        .map_err(|_| JiraCredentialError::new(JiraCredentialErrorKind::Unavailable))
    }
}

type MapperCall = (String, JiraResourceKind, Option<String>);

#[derive(Clone, Default)]
struct MapperFixture(Arc<Mutex<Vec<MapperCall>>>);

impl JiraEventMapperPort for MapperFixture {
    fn map_event(
        &mut self,
        _authority: &winwincode_integration::ConnectorAuthority,
        event: &JiraInboundEvent<'_>,
    ) -> Result<NormalizedInboundEvent, ConnectorCallError> {
        self.0.lock().expect("mapper calls").push((
            event.event_type().to_owned(),
            event.resource_kind(),
            event.actor_account_id().map(str::to_owned),
        ));
        NormalizedInboundEvent::try_new(
            "delivery.external_reference.update",
            br#"{"command":"delivery.external_reference.update"}"#.to_vec(),
        )
        .map_err(|_| {
            ConnectorCallError::try_new(ConnectorCallErrorKind::Permanent, "JIRA_MAPPING_INVALID")
                .expect("mapping error")
        })
    }
}

fn webhook_payload(event_type: &str, timestamp: u64, resource_id: &str) -> Vec<u8> {
    let mut value = json!({
        "issue": {
            "fields": {"project": {"key": "WWC"}},
            "id": "issue-101",
            "key": "WWC-1"
        },
        "timestamp": timestamp,
        "user": {"accountId": "jira-user-7"},
        "webhookEvent": event_type
    });
    if event_type.starts_with("comment_") {
        value["comment"] = json!({"id": resource_id});
    } else {
        value["issue"]["id"] = Value::String(resource_id.to_owned());
    }
    serde_json::to_vec(&value).expect("Jira webhook JSON")
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

fn jira_webhook(
    factory: &JiraWebhookRequestFactory,
    event_id: &str,
    payload: Vec<u8>,
    received_at: u64,
) -> winwincode_integration::InboundWebhookRequest {
    let signature = webhook_signature(&payload);
    factory
        .build(
            tenant_scope(),
            JiraWebhookHeaders::try_new(event_id, signature).expect("headers"),
            payload,
            received_at,
        )
        .expect("Jira webhook")
}

#[test]
fn signed_closed_webhook_set_is_scoped_ordered_replayed_and_secret_safe() {
    let root = temporary_directory("webhooks");
    let config = base_config(
        "https://api.atlassian.com/ex/jira/site-fixture-1".to_owned(),
        JiraTlsRoots::WebPki,
    );
    let factory = JiraWebhookRequestFactory::new(config.clone());
    let credentials = CredentialFixture::full();
    let webhook_lookups = Arc::clone(&credentials.webhook_lookups);
    let mut verifier = JiraWebhookVerifier::new(config.clone(), credentials.clone());
    let mapper = MapperFixture::default();
    let mapper_calls = Arc::clone(&mapper.0);
    let mut connector =
        JiraEnterpriseConnector::try_new(config.clone(), credentials, mapper, FixedClock(100))
            .expect("connector");
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);

    let cases = [
        ("jira:issue_created", "issue-101", JiraResourceKind::Issue),
        ("jira:issue_updated", "issue-102", JiraResourceKind::Issue),
        ("jira:issue_deleted", "issue-103", JiraResourceKind::Issue),
        ("comment_created", "comment-201", JiraResourceKind::Comment),
        ("comment_updated", "comment-202", JiraResourceKind::Comment),
        ("comment_deleted", "comment-203", JiraResourceKind::Comment),
    ];
    for (index, (event_type, resource_id, _kind)) in cases.iter().enumerate() {
        let timestamp = 100 + u64::try_from(index).expect("small index");
        let request = jira_webhook(
            &factory,
            &format!("event-{index}"),
            webhook_payload(event_type, timestamp, resource_id),
            20 + timestamp,
        );
        assert_eq!(
            framework
                .receive_webhook(&request, &mut verifier, &mut connector)
                .expect("closed event")
                .status(),
            InboundStatus::Accepted
        );
    }
    let replay = jira_webhook(
        &factory,
        "event-0",
        webhook_payload("jira:issue_created", 100, "issue-101"),
        120,
    );
    assert!(
        framework
            .receive_webhook(&replay, &mut verifier, &mut connector)
            .expect("exact replay")
            .idempotent_replay()
    );
    let stale = jira_webhook(
        &factory,
        "event-stale",
        webhook_payload("jira:issue_updated", 99, "issue-101"),
        121,
    );
    assert_eq!(
        framework
            .receive_webhook(&stale, &mut verifier, &mut connector)
            .expect("stale event")
            .status(),
        InboundStatus::IgnoredOutOfOrder
    );
    assert_eq!(mapper_calls.lock().expect("mapper calls").len(), 8);
    assert_eq!(webhook_lookups.load(Ordering::Relaxed), 8);
    assert_eq!(
        mapper_calls.lock().expect("mapper calls")[0].2.as_deref(),
        Some("jira-user-7")
    );

    assert_bad_signature_rejected(&factory, &mut framework, &mut verifier, &mut connector);
    assert_wrong_project_rejected(&factory);
    assert_wrong_site_envelope_rejected(&factory);
    let audit = serde_json::to_string(
        &framework
            .storage()
            .audit_facts(&tenant_scope(), config.integration_id(), 0, 50)
            .expect("audit"),
    )
    .expect("audit JSON");
    assert!(!audit.contains(std::str::from_utf8(WEBHOOK_SECRET).expect("secret UTF-8")));
    assert!(!audit.contains("event-0"));
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

fn assert_bad_signature_rejected(
    factory: &JiraWebhookRequestFactory,
    framework: &mut IntegrationFramework,
    verifier: &mut JiraWebhookVerifier<CredentialFixture>,
    connector: &mut JiraEnterpriseConnector<CredentialFixture, MapperFixture, FixedClock>,
) {
    let payload = webhook_payload("jira:issue_updated", 201, "issue-301");
    let request = factory
        .build(
            tenant_scope(),
            JiraWebhookHeaders::try_new(
                "bad-signature",
                b"sha256=0000000000000000000000000000000000000000000000000000000000000000",
            )
            .expect("headers"),
            payload,
            301,
        )
        .expect("request");
    assert_eq!(
        framework
            .receive_webhook(&request, verifier, connector)
            .expect_err("signature rejection")
            .kind(),
        IntegrationErrorKind::SignatureRejected
    );
}

fn assert_wrong_project_rejected(factory: &JiraWebhookRequestFactory) {
    let mut value: Value =
        serde_json::from_slice(&webhook_payload("jira:issue_updated", 200, "issue-300"))
            .expect("payload");
    value["issue"]["fields"]["project"]["key"] = Value::String("OTHER".to_owned());
    let payload = serde_json::to_vec(&value).expect("JSON");
    assert_eq!(
        factory
            .build(
                tenant_scope(),
                JiraWebhookHeaders::try_new("wrong-project", webhook_signature(&payload))
                    .expect("headers"),
                payload,
                300,
            )
            .expect_err("project scope mismatch")
            .kind(),
        IntegrationErrorKind::Invalid
    );
}

fn assert_wrong_site_envelope_rejected(factory: &JiraWebhookRequestFactory) {
    let mut value: Value =
        serde_json::from_slice(&webhook_payload("jira:issue_updated", 200, "issue-300"))
            .expect("payload");
    value["siteId"] = Value::String("foreign-site".to_owned());
    let payload = serde_json::to_vec(&value).expect("JSON");
    assert_eq!(
        factory
            .build(
                tenant_scope(),
                JiraWebhookHeaders::try_new("wrong-site", webhook_signature(&payload))
                    .expect("headers"),
                payload,
                300,
            )
            .expect_err("site scope mismatch")
            .kind(),
        IntegrationErrorKind::Invalid
    );
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

struct TlsJiraFixture {
    endpoint: String,
    certificate_der: Vec<u8>,
    requests: mpsc::Receiver<Vec<u8>>,
    request_count: usize,
    server: thread::JoinHandle<()>,
}

impl TlsJiraFixture {
    fn start(replies: Vec<HttpReply>) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("generate Jira TLS certificate");
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .expect("Jira TLS server config");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind Jira TLS fixture");
        let address = listener.local_addr().expect("Jira TLS address");
        let request_count = replies.len();
        let (sender, requests) = mpsc::channel();
        let server = thread::spawn(move || {
            for reply in replies {
                let (socket, _) = listener.accept().expect("accept Jira TLS request");
                let connection =
                    ServerConnection::new(Arc::new(config.clone())).expect("Jira TLS connection");
                let mut stream = StreamOwned::new(connection, socket);
                sender
                    .send(read_http_request(&mut stream))
                    .expect("record Jira request");
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
        self.server.join().expect("join Jira TLS fixture");
        (0..self.request_count)
            .map(|_| self.requests.recv().expect("captured Jira request"))
            .collect()
    }
}

fn read_http_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).expect("read Jira request");
        assert_ne!(count, 0, "Jira request closed before body");
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
        204 => "No Content",
        429 => "Too Many Requests",
        _ => "Unprocessable Entity",
    };
    write!(
        stream,
        "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    )
    .expect("write Jira response headers");
    for (name, value) in &reply.headers {
        write!(stream, "{name}: {value}\r\n").expect("write Jira response header");
    }
    stream.write_all(b"\r\n").expect("end Jira headers");
    stream
        .write_all(&reply.body)
        .expect("write Jira response body");
    stream.flush().expect("flush Jira response");
}

fn outbound_request(
    config: &JiraConnectorConfig,
    operation_key: &str,
    operation_name: &str,
    payload: &Value,
) -> OutboundRequest {
    OutboundRequest::try_new(
        config.integration_id().clone(),
        tenant_scope(),
        IntegrationOperationKey::derive(operation_key).expect("operation key"),
        operation_name,
        serde_json::to_vec(payload).expect("operation JSON"),
        RetryPolicy::try_new(3, 10, 40).expect("retry policy"),
        100,
    )
    .expect("outbound request")
}

fn integration_lease(tail: char) -> IntegrationLeaseId {
    IntegrationLeaseId::try_new(id("igl", tail)).expect("lease id")
}

fn build_connector(
    config: JiraConnectorConfig,
    credentials: CredentialFixture,
) -> JiraEnterpriseConnector<CredentialFixture, MapperFixture, FixedClock> {
    JiraEnterpriseConnector::try_new(
        config,
        credentials,
        MapperFixture::default(),
        FixedClock(100),
    )
    .expect("connector")
}

#[test]
fn issue_create_recovers_by_property_marker_without_a_second_remote_write() {
    let operation_key =
        IntegrationOperationKey::derive("jira-issue-create").expect("operation key");
    let fixture = TlsJiraFixture::start(vec![
        HttpReply::json(200, &json!({"issues": []})),
        HttpReply::json(201, &json!({"id": "10001", "key": "WWC-1"})),
        HttpReply::json(200, &json!({"issues": [{"id": "10001", "key": "WWC-1"}]})),
    ]);
    let root = temporary_directory("issue-recovery");
    let config = base_config(
        fixture.endpoint.clone(),
        JiraTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    let request = outbound_request(
        &config,
        "jira-issue-create",
        "jira.issue.create.v1",
        &json!({
            "description": null,
            "issue_type": "Task",
            "summary": "Implement durable delivery"
        }),
    );
    assert_eq!(request.operation_key(), &operation_key);
    let credentials = CredentialFixture::full();
    let token_lookups = Arc::clone(&credentials.token_lookups);
    let mut connector = build_connector(config.clone(), credentials);
    let mut first = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut first, &config);
    first.enqueue_outbound(&request).expect("enqueue");
    let abandoned = first
        .storage_mut()
        .claim_due(
            &tenant_scope(),
            config.integration_id(),
            100,
            integration_lease('A'),
            105,
        )
        .expect("claim")
        .expect("due claim");
    assert!(
        winwincode_integration::ConnectorPort::deliver_outbound(&mut connector, &abandoned)
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
            integration_lease('B'),
            110,
        )
        .expect("recover claim")
        .expect("due recovery");
    let found = winwincode_integration::ConnectorPort::deliver_outbound(&mut connector, &recovered)
        .expect("marker lookup");
    assert!(!found.remote_write_performed());
    restarted
        .storage_mut()
        .record_success(&tenant_scope(), &recovered, &found, 106)
        .expect("record recovered success");

    let requests = fixture.finish();
    assert_issue_create_requests(&requests, &operation_key);
    assert_eq!(token_lookups.load(Ordering::Relaxed), 2);
    assert_eq!(
        restarted
            .outbound_operation(&tenant_scope(), config.integration_id(), &operation_key)
            .expect("operation")
            .state(),
        OutboundOperationState::Delivered
    );
    let database = fs::read(restarted.storage().database_path()).expect("database bytes");
    assert!(find_bytes(&database, OAUTH_TOKEN.as_bytes()).is_none());
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

fn assert_issue_create_requests(requests: &[Vec<u8>], operation_key: &IntegrationOperationKey) {
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with(b"POST "))
            .count(),
        1
    );
    let create = String::from_utf8_lossy(&requests[1]);
    assert!(create.to_ascii_lowercase().contains(&format!(
        "authorization: bearer {}",
        OAUTH_TOKEN.to_ascii_lowercase()
    )));
    let body: Value = serde_json::from_slice(http_request_body(&requests[1])).expect("create body");
    assert_eq!(
        body["properties"][0]["value"]["key"].as_str(),
        Some(operation_key.digest().0.as_str())
    );
    let digest = operation_key
        .digest()
        .0
        .strip_prefix("sha256:")
        .expect("digest prefix");
    let marker = format!("wwc-{digest}");
    assert_eq!(body["fields"]["labels"][0].as_str(), Some(marker.as_str()));
}

#[test]
fn comment_create_carries_the_same_durable_marker_and_oauth_scope() {
    let fixture = TlsJiraFixture::start(vec![
        HttpReply::json(
            200,
            &json!({"comments": [], "maxResults": 100, "startAt": 0}),
        ),
        HttpReply::json(201, &json!({"id": "20001"})),
    ]);
    let root = temporary_directory("comment-create");
    let config = base_config(
        fixture.endpoint.clone(),
        JiraTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    let request = outbound_request(
        &config,
        "jira-comment-create",
        "jira.comment.create.v1",
        &json!({
            "body": {"content": [], "type": "doc", "version": 1},
            "issue_key": "WWC-7"
        }),
    );
    let mut connector = build_connector(config.clone(), CredentialFixture::full());
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);
    framework.enqueue_outbound(&request).expect("enqueue");
    let OutboundAttemptResult::Delivered(receipt) = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            100,
            integration_lease('G'),
            105,
            &mut connector,
        )
        .expect("comment delivery")
        .expect("due comment")
    else {
        panic!("expected delivered comment");
    };
    assert_eq!(receipt.remote_write_performed(), Some(true));
    let requests = fixture.finish();
    assert!(String::from_utf8_lossy(&requests[0]).starts_with(
        "GET /rest/api/3/issue/WWC-7/comment?expand=properties&maxResults=100&startAt=0"
    ));
    let body: Value =
        serde_json::from_slice(http_request_body(&requests[1])).expect("comment body");
    assert_eq!(
        body["properties"][0]["value"]["key"].as_str(),
        Some(request.operation_key().digest().0.as_str())
    );
    assert_eq!(body["body"]["type"].as_str(), Some("doc"));
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn rate_limit_uses_provider_floor_and_permission_or_revocation_dead_letters() {
    let rate_limit = TlsJiraFixture::start(vec![HttpReply {
        status: 429,
        headers: vec![("Retry-After".to_owned(), "30".to_owned())],
        body: Vec::new(),
    }]);
    let root = temporary_directory("rate-limit");
    let config = base_config(
        rate_limit.endpoint.clone(),
        JiraTlsRoots::Specific(vec![rate_limit.certificate_der.clone()]),
    );
    let mut connector = build_connector(config.clone(), CredentialFixture::full());
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);
    let request = outbound_request(
        &config,
        "rate-limited-issue",
        "jira.issue.create.v1",
        &json!({"description": null, "issue_type": "Bug", "summary": "Retry"}),
    );
    framework.enqueue_outbound(&request).expect("enqueue");
    let OutboundAttemptResult::RetryScheduled(operation) = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            100,
            integration_lease('C'),
            105,
            &mut connector,
        )
        .expect("rate limit")
        .expect("attempt")
    else {
        panic!("expected retry schedule");
    };
    assert_eq!(operation.eligible_at_millis(), 30_100);
    assert_eq!(rate_limit.finish().len(), 1);
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup rate limit");

    assert_dead_letter_for_credentials(
        "permission",
        CredentialFixture {
            scopes: vec![JiraOAuthScope::ReadIssue],
            ..CredentialFixture::full()
        },
        ConnectorState::Active,
    );
    assert_dead_letter_for_credentials(
        "revoked",
        CredentialFixture {
            revoked: true,
            ..CredentialFixture::full()
        },
        ConnectorState::CredentialRevoked,
    );
}

fn assert_dead_letter_for_credentials(
    name: &str,
    credentials: CredentialFixture,
    expected_authority_state: ConnectorState,
) {
    let root = temporary_directory(name);
    let config = base_config(
        "https://api.atlassian.com/ex/jira/site-fixture-1".to_owned(),
        JiraTlsRoots::WebPki,
    );
    let mut connector = build_connector(config.clone(), credentials);
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);
    let request = outbound_request(
        &config,
        name,
        "jira.issue.create.v1",
        &json!({"description": null, "issue_type": "Story", "summary": "Denied"}),
    );
    framework.enqueue_outbound(&request).expect("enqueue");
    assert!(matches!(
        framework
            .deliver_next(
                &tenant_scope(),
                config.integration_id(),
                100,
                integration_lease('D'),
                105,
                &mut connector,
            )
            .expect("attempt")
            .expect("due operation"),
        OutboundAttemptResult::DeadLettered(_)
    ));
    assert_eq!(
        framework
            .storage()
            .authority(&tenant_scope(), config.integration_id())
            .expect("authority")
            .state(),
        expected_authority_state
    );
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn config_scope_and_closed_operations_reject_cross_project_or_delete_calls() {
    for endpoint in [
        "http://jira.example",
        "https://token@jira.example",
        "https://jira.example?token=secret",
        "https://jira.example/#fragment",
        "https://jira.example",
        "https://api.atlassian.com/ex/jira/foreign-site",
    ] {
        assert_eq!(
            JiraConnectorConfig::try_new(
                integration_id(),
                credential_reference_id(),
                site_id(),
                project_key(),
                endpoint,
                JiraTlsRoots::WebPki,
            )
            .expect_err("invalid endpoint")
            .kind(),
            IntegrationErrorKind::Invalid
        );
    }
    assert_eq!(
        JiraConnectorConfig::try_new(
            integration_id(),
            credential_reference_id(),
            site_id(),
            project_key(),
            "https://jira.example:8443",
            JiraTlsRoots::Specific(vec![vec![1]]),
        )
        .expect_err("custom TLS roots are loopback-fixture only")
        .kind(),
        IntegrationErrorKind::Invalid
    );
    let root = temporary_directory("closed-operation");
    let config = base_config(
        "https://api.atlassian.com/ex/jira/site-fixture-1".to_owned(),
        JiraTlsRoots::WebPki,
    );
    let mut connector = build_connector(config.clone(), CredentialFixture::full());
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);
    for (index, (operation, payload)) in [
        ("jira.issue.delete.v1", json!({"issue_key": "WWC-1"})),
        (
            "jira.comment.create.v1",
            json!({"body": {"type": "doc"}, "issue_key": "OTHER-1"}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let request = outbound_request(&config, &format!("rejected-{index}"), operation, &payload);
        framework.enqueue_outbound(&request).expect("enqueue");
        assert!(matches!(
            framework
                .deliver_next(
                    &tenant_scope(),
                    config.integration_id(),
                    100,
                    integration_lease(char::from(b'E' + u8::try_from(index).expect("small index"))),
                    105,
                    &mut connector,
                )
                .expect("attempt")
                .expect("due operation"),
            OutboundAttemptResult::DeadLettered(_)
        ));
    }
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

const LIVE_GATE_ENV: &str = "WINWINCODE_JIRA_LIVE_TEST";
const LIVE_REVOKED_GATE_ENV: &str = "WINWINCODE_JIRA_LIVE_REVOKED_TEST";
const LIVE_TOKEN_FILE_ENV: &str = "WINWINCODE_JIRA_LIVE_OAUTH_TOKEN_FILE";
const LIVE_WEBHOOK_SECRET_FILE_ENV: &str = "WINWINCODE_JIRA_LIVE_WEBHOOK_SECRET_FILE";
const LIVE_SITE_ENV: &str = "WINWINCODE_JIRA_LIVE_SITE_ID";
const LIVE_PROJECT_ENV: &str = "WINWINCODE_JIRA_LIVE_PROJECT_KEY";
const LIVE_ISSUE_TYPE_ENV: &str = "WINWINCODE_JIRA_LIVE_ISSUE_TYPE";
const LIVE_WEBHOOK_CAPTURE_FILE_ENV: &str = "WINWINCODE_JIRA_LIVE_WEBHOOK_CAPTURE_FILE";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveWebhookEvidenceFile {
    site_id: String,
    project_key: String,
    captures: Vec<LiveWebhookCapture>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveWebhookCapture {
    event_id: String,
    signature: String,
    received_at_millis: u64,
    raw_body: String,
}

impl LiveWebhookEvidenceFile {
    fn parse(
        bytes: &[u8],
        site_id: &JiraSiteId,
        project_key: &JiraProjectKey,
        token: &[u8],
        secret: &[u8],
    ) -> Result<Self, &'static str> {
        if bytes.is_empty()
            || bytes.len() > 16 * 1_024 * 1_024
            || find_bytes(bytes, token).is_some()
            || find_bytes(bytes, secret).is_some()
        {
            return Err("live Jira webhook evidence is invalid");
        }
        let evidence: Self =
            serde_json::from_slice(bytes).map_err(|_| "live Jira webhook evidence is invalid")?;
        if evidence.site_id != site_id.as_str()
            || evidence.project_key != project_key.as_str()
            || evidence.captures.len() != 6
        {
            return Err("live Jira webhook evidence scope is invalid");
        }
        let mut event_ids = BTreeSet::new();
        let mut event_types = BTreeSet::new();
        let mut issue_ids = BTreeSet::new();
        let mut comment_ids = BTreeSet::new();
        let mut issue_times = Vec::new();
        let mut comment_times = Vec::new();
        for capture in &evidence.captures {
            JiraWebhookHeaders::try_new(&capture.event_id, capture.signature.as_bytes())
                .map_err(|_| "live Jira webhook evidence headers are invalid")?;
            if !event_ids.insert(capture.event_id.as_str()) || capture.received_at_millis == 0 {
                return Err("live Jira webhook evidence identity is invalid");
            }
            let payload: Value = serde_json::from_str(&capture.raw_body)
                .map_err(|_| "live Jira webhook evidence body is invalid")?;
            let event_type = payload
                .get("webhookEvent")
                .and_then(Value::as_str)
                .ok_or("live Jira webhook evidence event is invalid")?;
            let timestamp = payload
                .get("timestamp")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0 && *value <= capture.received_at_millis)
                .ok_or("live Jira webhook evidence timestamp is invalid")?;
            let project = payload
                .pointer("/issue/fields/project/key")
                .and_then(Value::as_str);
            if project != Some(project_key.as_str()) || !event_types.insert(event_type.to_owned()) {
                return Err("live Jira webhook evidence event scope is invalid");
            }
            match event_type {
                "jira:issue_created" | "jira:issue_updated" | "jira:issue_deleted" => {
                    let resource_id = payload
                        .pointer("/issue/id")
                        .and_then(Value::as_str)
                        .ok_or("live Jira issue evidence is invalid")?;
                    issue_ids.insert(resource_id.to_owned());
                    issue_times.push((event_type.to_owned(), timestamp));
                }
                "comment_created" | "comment_updated" | "comment_deleted" => {
                    let resource_id = payload
                        .pointer("/comment/id")
                        .and_then(Value::as_str)
                        .ok_or("live Jira comment evidence is invalid")?;
                    comment_ids.insert(resource_id.to_owned());
                    comment_times.push((event_type.to_owned(), timestamp));
                }
                _ => return Err("live Jira webhook evidence event is unsupported"),
            }
        }
        let expected = [
            "jira:issue_created",
            "jira:issue_updated",
            "jira:issue_deleted",
            "comment_created",
            "comment_updated",
            "comment_deleted",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
        if event_types != expected
            || issue_ids.len() != 1
            || comment_ids.len() != 1
            || !lifecycle_is_ordered(&issue_times, "jira:issue")
            || !lifecycle_is_ordered(&comment_times, "comment")
        {
            return Err("live Jira webhook evidence lifecycle is invalid");
        }
        Ok(evidence)
    }
}

fn lifecycle_is_ordered(values: &[(String, u64)], prefix: &str) -> bool {
    let suffixes = ["_created", "_updated", "_deleted"];
    let mut prior = 0;
    for suffix in suffixes {
        let event_type = format!("{prefix}{suffix}");
        let Some(timestamp) = values
            .iter()
            .find_map(|(name, timestamp)| (name == &event_type).then_some(*timestamp))
        else {
            return false;
        };
        if timestamp <= prior {
            return false;
        }
        prior = timestamp;
    }
    true
}

struct LiveJiraGateConfig {
    token: Vec<u8>,
    webhook_secret: Vec<u8>,
    site_id: JiraSiteId,
    project_key: JiraProjectKey,
    issue_type: String,
    webhook_evidence: Option<LiveWebhookEvidenceFile>,
}

impl LiveJiraGateConfig {
    fn load(gate_environment: &str, require_webhook_evidence: bool) -> Self {
        assert_eq!(
            std::env::var(gate_environment).as_deref(),
            Ok("1"),
            "set the explicit live Jira gate to 1"
        );
        let token_path = required_live_path(LIVE_TOKEN_FILE_ENV);
        let secret_path = required_live_path(LIVE_WEBHOOK_SECRET_FILE_ENV);
        assert_ne!(
            fs::canonicalize(&token_path).expect("canonical live Jira OAuth token file"),
            fs::canonicalize(&secret_path).expect("canonical live Jira webhook secret file"),
            "live Jira OAuth and webhook material must use separate files"
        );
        let token = read_private_live_file(&token_path, 4_096)
            .unwrap_or_else(|message| panic!("{LIVE_TOKEN_FILE_ENV}: {message}"));
        let webhook_secret = read_private_live_file(&secret_path, 4_096)
            .unwrap_or_else(|message| panic!("{LIVE_WEBHOOK_SECRET_FILE_ENV}: {message}"));
        JiraWebhookSecret::try_new(&webhook_secret).expect("valid live Jira webhook secret");
        let site_id = JiraSiteId::try_new(required_live_value(LIVE_SITE_ENV))
            .expect("valid live Jira site identity");
        let project_key = JiraProjectKey::try_new(required_live_value(LIVE_PROJECT_ENV))
            .expect("valid live Jira project key");
        let issue_type = required_live_value(LIVE_ISSUE_TYPE_ENV);
        assert!(
            matches!(issue_type.as_str(), "Task" | "Bug" | "Story"),
            "live Jira issue type must be Task, Bug, or Story"
        );
        JiraOAuthAccessToken::try_new(
            &token,
            site_id.clone(),
            [project_key.clone()],
            [
                JiraOAuthScope::ReadIssue,
                JiraOAuthScope::WriteIssue,
                JiraOAuthScope::ReadComment,
                JiraOAuthScope::WriteComment,
            ],
            current_time_millis() + 60_000,
        )
        .expect("valid live Jira OAuth token material");
        let webhook_evidence = require_webhook_evidence.then(|| {
            let path = required_live_path(LIVE_WEBHOOK_CAPTURE_FILE_ENV);
            let bytes = read_private_live_file(&path, 16 * 1_024 * 1_024)
                .unwrap_or_else(|message| panic!("{LIVE_WEBHOOK_CAPTURE_FILE_ENV}: {message}"));
            LiveWebhookEvidenceFile::parse(&bytes, &site_id, &project_key, &token, &webhook_secret)
                .expect("valid live Jira webhook evidence")
        });
        Self {
            token,
            webhook_secret,
            site_id,
            project_key,
            issue_type,
            webhook_evidence,
        }
    }
}

impl Drop for LiveJiraGateConfig {
    fn drop(&mut self) {
        self.token.fill(0);
        self.webhook_secret.fill(0);
    }
}

fn required_live_value(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("{name} is required for the live Jira gate"))
}

fn required_live_path(name: &str) -> PathBuf {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map_or_else(
            || panic!("{name} is required for the live Jira gate"),
            PathBuf::from,
        )
}

fn read_private_live_file(path: &Path, maximum: usize) -> Result<Vec<u8>, &'static str> {
    let metadata = fs::metadata(path).map_err(|_| "file is not readable")?;
    if !metadata.is_file() {
        return Err("path is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("file grants group or other permissions");
        }
    }
    let bytes = fs::read(path).map_err(|_| "file is not readable")?;
    if bytes.is_empty() || bytes.len() > maximum {
        return Err("file size is invalid");
    }
    Ok(bytes)
}

fn current_time_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_millis(),
    )
    .expect("current time fits u64")
}

struct LiveCredentialFixture {
    token: Vec<u8>,
    webhook_secret: Vec<u8>,
    site_id: JiraSiteId,
    project_key: JiraProjectKey,
    expires_at_millis: u64,
    revoked: Arc<AtomicBool>,
}

impl LiveCredentialFixture {
    fn new(config: &LiveJiraGateConfig, expires_at_millis: u64) -> Self {
        Self {
            token: config.token.clone(),
            webhook_secret: config.webhook_secret.clone(),
            site_id: config.site_id.clone(),
            project_key: config.project_key.clone(),
            expires_at_millis,
            revoked: Arc::new(AtomicBool::new(false)),
        }
    }

    fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
    }
}

impl Clone for LiveCredentialFixture {
    fn clone(&self) -> Self {
        Self {
            token: self.token.clone(),
            webhook_secret: self.webhook_secret.clone(),
            site_id: self.site_id.clone(),
            project_key: self.project_key.clone(),
            expires_at_millis: self.expires_at_millis,
            revoked: Arc::clone(&self.revoked),
        }
    }
}

impl Drop for LiveCredentialFixture {
    fn drop(&mut self) {
        self.token.fill(0);
        self.webhook_secret.fill(0);
    }
}

impl JiraCredentialPort for LiveCredentialFixture {
    fn resolve_webhook_secret(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<JiraWebhookSecret, JiraCredentialError> {
        if self.revoked.load(Ordering::SeqCst) {
            return Err(JiraCredentialError::new(JiraCredentialErrorKind::Revoked));
        }
        if reference != &credential_reference_id() {
            return Err(JiraCredentialError::new(
                JiraCredentialErrorKind::PermissionDenied,
            ));
        }
        JiraWebhookSecret::try_new(&self.webhook_secret)
            .map_err(|_| JiraCredentialError::new(JiraCredentialErrorKind::Unavailable))
    }

    fn resolve_oauth_token(
        &mut self,
        reference: &CredentialReferenceId,
        requested_site: &JiraSiteId,
    ) -> Result<JiraOAuthAccessToken, JiraCredentialError> {
        if self.revoked.load(Ordering::SeqCst) {
            return Err(JiraCredentialError::new(JiraCredentialErrorKind::Revoked));
        }
        if reference != &credential_reference_id() || requested_site != &self.site_id {
            return Err(JiraCredentialError::new(
                JiraCredentialErrorKind::PermissionDenied,
            ));
        }
        JiraOAuthAccessToken::try_new(
            &self.token,
            self.site_id.clone(),
            [self.project_key.clone()],
            [
                JiraOAuthScope::ReadIssue,
                JiraOAuthScope::WriteIssue,
                JiraOAuthScope::ReadComment,
                JiraOAuthScope::WriteComment,
            ],
            self.expires_at_millis,
        )
        .map_err(|_| JiraCredentialError::new(JiraCredentialErrorKind::Unavailable))
    }
}

struct LiveJiraClient {
    agent: ureq::Agent,
    api_base_url: String,
    token: Vec<u8>,
}

impl LiveJiraClient {
    fn new(config: &LiveJiraGateConfig) -> Self {
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .root_certs(ureq::tls::RootCerts::WebPki)
            .use_sni(true)
            .disable_verification(false)
            .build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .timeout_global(Some(Duration::from_secs(30)))
            .tls_config(tls)
            .build()
            .into();
        Self {
            agent,
            api_base_url: format!(
                "https://api.atlassian.com/ex/jira/{}/",
                config.site_id.as_str()
            ),
            token: config.token.clone(),
        }
    }

    fn accessible_resources(&self) -> LiveHttpResponse {
        self.request_url(
            "GET",
            "https://api.atlassian.com/oauth/token/accessible-resources",
            None,
        )
    }

    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> LiveHttpResponse {
        self.request_url(
            method,
            &format!("{}{}", self.api_base_url, path.trim_start_matches('/')),
            body,
        )
    }

    fn request_url(&self, method: &str, url: &str, body: Option<&Value>) -> LiveHttpResponse {
        let authorization = format!(
            "Bearer {}",
            std::str::from_utf8(&self.token).expect("validated live Jira token")
        );
        let response = match (method, body) {
            ("GET", None) => self
                .agent
                .get(url)
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .call(),
            ("DELETE", None) => self
                .agent
                .delete(url)
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .call(),
            ("POST", Some(value)) => self
                .agent
                .post(url)
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .send_json(value),
            ("PUT", Some(value)) => self
                .agent
                .put(url)
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .send_json(value),
            _ => panic!("invalid live Jira request shape"),
        }
        .unwrap_or_else(|_| panic!("live Jira transport failed"));
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .with_config()
            .limit(2 * 1_024 * 1_024)
            .read_to_vec()
            .expect("bounded live Jira response");
        let body = if bytes.is_empty() {
            None
        } else {
            serde_json::from_slice(&bytes).ok()
        };
        LiveHttpResponse { status, body }
    }

    fn require_json(&self, path: &str, accepted: &[u16]) -> Value {
        let response = self.request("GET", path, None);
        assert!(
            accepted.contains(&response.status),
            "live Jira GET returned status {}",
            response.status
        );
        response.body.expect("live Jira JSON response")
    }

    fn issue_keys_for_marker(&self, project_key: &JiraProjectKey, marker: &str) -> Vec<String> {
        let jql = format!("project={} AND labels=\"{marker}\"", project_key.as_str());
        let body = self.require_json(
            &format!(
                "rest/api/3/search/jql?jql={}&maxResults=2",
                encode_live_query(&jql)
            ),
            &[200],
        );
        body.get("issues")
            .and_then(Value::as_array)
            .expect("live Jira issue search results")
            .iter()
            .filter_map(|issue| issue.get("key").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    fn comment_ids_for_marker(&self, issue_key: &str, operation_key: &str) -> Vec<String> {
        let body = self.require_json(
            &format!(
                "rest/api/3/issue/{issue_key}/comment?expand=properties&maxResults=100&startAt=0"
            ),
            &[200],
        );
        body.get("comments")
            .and_then(Value::as_array)
            .expect("live Jira comment list")
            .iter()
            .filter(|comment| {
                comment
                    .get("properties")
                    .and_then(Value::as_array)
                    .is_some_and(|properties| {
                        properties.iter().any(|property| {
                            property.get("key").and_then(Value::as_str)
                                == Some("winwincode.operation")
                                && property.pointer("/value/key").and_then(Value::as_str)
                                    == Some(operation_key)
                        })
                    })
            })
            .filter_map(|comment| comment.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }
}

impl Clone for LiveJiraClient {
    fn clone(&self) -> Self {
        Self {
            agent: self.agent.clone(),
            api_base_url: self.api_base_url.clone(),
            token: self.token.clone(),
        }
    }
}

impl Drop for LiveJiraClient {
    fn drop(&mut self) {
        self.token.fill(0);
    }
}

struct LiveHttpResponse {
    status: u16,
    body: Option<Value>,
}

struct LiveIssueCleanup {
    client: LiveJiraClient,
    issue_key: Option<String>,
}

impl LiveIssueCleanup {
    fn new(client: LiveJiraClient, issue_key: String) -> Self {
        Self {
            client,
            issue_key: Some(issue_key),
        }
    }

    fn disarm(&mut self) {
        self.issue_key = None;
    }
}

impl Drop for LiveIssueCleanup {
    fn drop(&mut self) {
        if let Some(issue_key) = &self.issue_key {
            let _ = self
                .client
                .request("DELETE", &format!("rest/api/3/issue/{issue_key}"), None);
        }
    }
}

fn encode_live_query(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            write!(output, "%{byte:02X}").expect("write live Jira query");
        }
    }
    output
}

fn live_operation_marker(operation_key: &IntegrationOperationKey) -> String {
    format!(
        "wwc-{}",
        operation_key
            .digest()
            .0
            .strip_prefix("sha256:")
            .expect("operation digest prefix")
    )
}

fn live_adf(text: &str) -> Value {
    json!({
        "content": [{
            "content": [{"text": text, "type": "text"}],
            "type": "paragraph"
        }],
        "type": "doc",
        "version": 1
    })
}

fn assert_directory_omits(root: &Path, needle: &[u8]) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(&path).expect("read live Jira gate directory") {
            let entry = entry.expect("live Jira gate entry");
            let file_type = entry.file_type().expect("live Jira gate file type");
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                let bytes = fs::read(entry.path()).expect("read live Jira gate artifact");
                assert!(
                    find_bytes(&bytes, needle).is_none(),
                    "live Jira gate artifact contains protected material"
                );
            }
        }
    }
}

#[test]
fn live_gate_files_scope_and_official_webhook_evidence_are_fail_closed() {
    let root = temporary_directory("live-gate-config");
    fs::create_dir_all(&root).expect("create live gate config fixture");
    let token_path = root.join("oauth-token");
    let secret_path = root.join("webhook-secret");
    fs::write(&token_path, OAUTH_TOKEN).expect("write OAuth fixture");
    fs::write(&secret_path, WEBHOOK_SECRET).expect("write webhook fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600))
            .expect("protect OAuth fixture");
        fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o600))
            .expect("protect webhook fixture");
    }
    assert_eq!(
        read_private_live_file(&token_path, 4_096).expect("private OAuth file"),
        OAUTH_TOKEN.as_bytes()
    );
    assert_eq!(
        read_private_live_file(&secret_path, 4_096).expect("private webhook file"),
        WEBHOOK_SECRET
    );

    let evidence = live_webhook_evidence_json("site-fixture-1", "WWC");
    let parsed = LiveWebhookEvidenceFile::parse(
        &evidence,
        &site_id(),
        &project_key(),
        OAUTH_TOKEN.as_bytes(),
        WEBHOOK_SECRET,
    )
    .expect("closed live webhook evidence");
    assert_eq!(parsed.captures.len(), 6);
    assert!(
        LiveWebhookEvidenceFile::parse(
            &live_webhook_evidence_json("foreign-site", "WWC"),
            &site_id(),
            &project_key(),
            OAUTH_TOKEN.as_bytes(),
            WEBHOOK_SECRET,
        )
        .is_err()
    );
    let mut leaked: Value = serde_json::from_slice(&evidence).expect("evidence JSON");
    leaked["credential"] = Value::String(OAUTH_TOKEN.to_owned());
    assert!(
        LiveWebhookEvidenceFile::parse(
            &serde_json::to_vec(&leaked).expect("leaked evidence JSON"),
            &site_id(),
            &project_key(),
            OAUTH_TOKEN.as_bytes(),
            WEBHOOK_SECRET,
        )
        .is_err()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o644))
            .expect("make OAuth fixture unsafe");
        assert!(read_private_live_file(&token_path, 4_096).is_err());
    }
    fs::remove_dir_all(root).expect("remove live gate config fixture");
}

fn live_webhook_evidence_json(site: &str, project: &str) -> Vec<u8> {
    let cases = [
        ("jira:issue_created", 1_000, "live-issue", "live-comment"),
        ("jira:issue_updated", 2_000, "live-issue", "live-comment"),
        ("comment_created", 3_000, "live-issue", "live-comment"),
        ("comment_updated", 4_000, "live-issue", "live-comment"),
        ("comment_deleted", 5_000, "live-issue", "live-comment"),
        ("jira:issue_deleted", 6_000, "live-issue", "live-comment"),
    ];
    let captures = cases
        .into_iter()
        .enumerate()
        .map(|(index, (event_type, timestamp, issue_id, comment_id))| {
            let payload = json!({
                "comment": {"id": comment_id},
                "issue": {
                    "fields": {"project": {"key": project}},
                    "id": issue_id,
                    "key": format!("{project}-1")
                },
                "timestamp": timestamp,
                "user": {"accountId": "live-user"},
                "webhookEvent": event_type
            });
            let raw_body = serde_json::to_string(&payload).expect("raw webhook body");
            json!({
                "event_id": format!("live-event-{index}"),
                "raw_body": raw_body,
                "received_at_millis": timestamp + 1,
                "signature": String::from_utf8(webhook_signature(raw_body.as_bytes()))
                    .expect("webhook signature UTF-8")
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_vec(&json!({
        "captures": captures,
        "project_key": project,
        "site_id": site
    }))
    .expect("live webhook evidence JSON")
}

fn validate_live_jira_access(client: &LiveJiraClient, config: &LiveJiraGateConfig) {
    let resources = client.accessible_resources();
    assert_eq!(
        resources.status, 200,
        "live Jira accessible-resources request must succeed"
    );
    let jira_resources = resources
        .body
        .and_then(|value| value.as_array().cloned())
        .expect("live Jira accessible resources")
        .into_iter()
        .filter(|resource| {
            resource
                .get("scopes")
                .and_then(Value::as_array)
                .is_some_and(|scopes| {
                    scopes
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|scope| scope.contains(":jira") || scope.ends_with(":jira-work"))
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        jira_resources.len(),
        1,
        "live Jira OAuth grant must expose exactly one Jira site"
    );
    let resource = &jira_resources[0];
    assert_eq!(
        resource.get("id").and_then(Value::as_str),
        Some(config.site_id.as_str()),
        "live Jira OAuth site does not match the configured site"
    );
    let scopes = resource
        .get("scopes")
        .and_then(Value::as_array)
        .expect("live Jira OAuth scopes")
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    assert!(
        scopes.contains("read:jira-work") && scopes.contains("write:jira-work"),
        "live Jira gate requires classic read:jira-work and write:jira-work scopes"
    );
    let project = client.request(
        "GET",
        &format!("rest/api/3/project/{}", config.project_key.as_str()),
        None,
    );
    assert_eq!(
        project.status, 200,
        "live Jira project must be visible to the configured OAuth grant"
    );
    assert_eq!(
        project
            .body
            .as_ref()
            .and_then(|value| value.get("key"))
            .and_then(Value::as_str),
        Some(config.project_key.as_str()),
        "live Jira project response crossed the configured project boundary"
    );
}

fn exercise_live_webhook_evidence(
    framework: &mut IntegrationFramework,
    config: &JiraConnectorConfig,
    credentials: &LiveCredentialFixture,
    evidence: &LiveWebhookEvidenceFile,
    now_millis: u64,
) -> winwincode_integration::InboundWebhookRequest {
    let factory = JiraWebhookRequestFactory::new(config.clone());
    let mut verifier = JiraWebhookVerifier::new(config.clone(), credentials.clone());
    let mapper = MapperFixture::default();
    let calls = Arc::clone(&mapper.0);
    let mut connector = JiraEnterpriseConnector::try_new(
        config.clone(),
        credentials.clone(),
        mapper,
        FixedClock(now_millis),
    )
    .expect("live Jira inbound connector");
    let mut captures = evidence.captures.iter().collect::<Vec<_>>();
    captures.sort_by_key(|capture| {
        serde_json::from_str::<Value>(&capture.raw_body)
            .ok()
            .and_then(|value| value.get("timestamp").and_then(Value::as_u64))
            .unwrap_or(u64::MAX)
    });
    let mut requests = Vec::new();
    for capture in captures {
        let request = factory
            .build(
                tenant_scope(),
                JiraWebhookHeaders::try_new(&capture.event_id, capture.signature.as_bytes())
                    .expect("live Jira webhook headers"),
                capture.raw_body.as_bytes().to_vec(),
                capture.received_at_millis,
            )
            .expect("live Jira webhook request");
        assert_eq!(
            framework
                .receive_webhook(&request, &mut verifier, &mut connector)
                .expect("authenticated live Jira webhook")
                .status(),
            InboundStatus::Accepted
        );
        requests.push(request);
    }
    assert_eq!(calls.lock().expect("live Jira mapper calls").len(), 6);
    assert!(
        framework
            .receive_webhook(&requests[0], &mut verifier, &mut connector)
            .expect("exact live Jira webhook replay")
            .idempotent_replay()
    );

    let first_capture = evidence
        .captures
        .iter()
        .find(|capture| capture.raw_body.contains("jira:issue_created"))
        .expect("live Jira issue-created capture");
    let bad_signature = factory
        .build(
            tenant_scope(),
            JiraWebhookHeaders::try_new(
                "live-bad-signature",
                b"sha256=0000000000000000000000000000000000000000000000000000000000000000",
            )
            .expect("bad live Jira signature header shape"),
            first_capture.raw_body.as_bytes().to_vec(),
            now_millis,
        )
        .expect("bad-signature live Jira request");
    assert_eq!(
        framework
            .receive_webhook(&bad_signature, &mut verifier, &mut connector)
            .expect_err("bad live Jira signature must fail")
            .kind(),
        IntegrationErrorKind::SignatureRejected
    );
    let stale = factory
        .build(
            tenant_scope(),
            JiraWebhookHeaders::try_new("live-stale-proof", first_capture.signature.as_bytes())
                .expect("stale live Jira headers"),
            first_capture.raw_body.as_bytes().to_vec(),
            now_millis,
        )
        .expect("stale live Jira request");
    assert_eq!(
        framework
            .receive_webhook(&stale, &mut verifier, &mut connector)
            .expect("stale live Jira webhook")
            .status(),
        InboundStatus::IgnoredOutOfOrder
    );
    requests.remove(0)
}

#[test]
#[ignore = "requires explicit Jira Cloud OAuth, admin-webhook, and capture-file live inputs"]
#[allow(clippy::too_many_lines)]
fn live_jira_cloud_issue_comment_webhook_retry_and_leak_gate() {
    let gate = LiveJiraGateConfig::load(LIVE_GATE_ENV, true);
    let now = current_time_millis();
    let api_base_url = format!(
        "https://api.atlassian.com/ex/jira/{}",
        gate.site_id.as_str()
    );
    let connector_config = JiraConnectorConfig::try_new(
        integration_id(),
        credential_reference_id(),
        gate.site_id.clone(),
        gate.project_key.clone(),
        api_base_url,
        JiraTlsRoots::WebPki,
    )
    .expect("live Jira connector configuration");
    let credentials = LiveCredentialFixture::new(&gate, now + 30 * 60 * 1_000);
    let client = LiveJiraClient::new(&gate);
    validate_live_jira_access(&client, &gate);
    let root = temporary_directory("live-cloud");
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &connector_config);
    let mut connector = JiraEnterpriseConnector::try_new(
        connector_config.clone(),
        credentials.clone(),
        MapperFixture::default(),
        FixedClock(now),
    )
    .expect("live Jira connector");

    let issue_seed = format!("jira-live-issue-{now}");
    let issue_create = outbound_request(
        &connector_config,
        &issue_seed,
        "jira.issue.create.v1",
        &json!({
            "description": live_adf("WinWinCode Jira live gate"),
            "issue_type": gate.issue_type.clone(),
            "summary": format!("WinWinCode live gate {now}")
        }),
    );
    framework
        .enqueue_outbound(&issue_create)
        .expect("enqueue live Jira issue");
    let issue_claim = framework
        .storage_mut()
        .claim_due(
            &tenant_scope(),
            connector_config.integration_id(),
            now,
            integration_lease('J'),
            now + 1,
        )
        .expect("claim live Jira issue")
        .expect("due live Jira issue");
    assert!(
        winwincode_integration::ConnectorPort::deliver_outbound(&mut connector, &issue_claim)
            .expect("create live Jira issue")
            .remote_write_performed()
    );
    let issue_marker = live_operation_marker(issue_create.operation_key());
    let issue_keys = client.issue_keys_for_marker(&gate.project_key, &issue_marker);
    assert_eq!(
        issue_keys.len(),
        1,
        "live Jira issue marker must identify exactly one issue"
    );
    let issue_key = issue_keys[0].clone();
    let mut cleanup = LiveIssueCleanup::new(client.clone(), issue_key.clone());
    let issue_property = client.require_json(
        &format!("rest/api/3/issue/{issue_key}/properties/winwincode.operation"),
        &[200],
    );
    assert_eq!(
        issue_property.pointer("/value/key").and_then(Value::as_str),
        Some(issue_create.operation_key().digest().0.as_str())
    );
    drop(framework);

    let mut framework =
        IntegrationFramework::new(IntegrationStorage::open(&root).expect("restart store"));
    let recovered_issue = framework
        .storage_mut()
        .claim_due(
            &tenant_scope(),
            connector_config.integration_id(),
            now + 1,
            integration_lease('K'),
            now + 2,
        )
        .expect("reclaim live Jira issue")
        .expect("abandoned live Jira issue");
    let issue_receipt =
        winwincode_integration::ConnectorPort::deliver_outbound(&mut connector, &recovered_issue)
            .expect("recover live Jira issue marker");
    assert!(!issue_receipt.remote_write_performed());
    framework
        .storage_mut()
        .record_success(&tenant_scope(), &recovered_issue, &issue_receipt, now + 1)
        .expect("record live Jira issue recovery");
    assert_eq!(
        client
            .issue_keys_for_marker(&gate.project_key, &issue_marker)
            .len(),
        1,
        "lost-response recovery must not create a second Jira issue"
    );

    let issue_summary = format!("WinWinCode live gate updated {now}");
    let issue_update = outbound_request(
        &connector_config,
        &format!("jira-live-issue-update-{now}"),
        "jira.issue.update.v1",
        &json!({
            "description": null,
            "issue_key": issue_key,
            "summary": issue_summary
        }),
    );
    framework
        .enqueue_outbound(&issue_update)
        .expect("enqueue live Jira issue update");
    assert!(matches!(
        framework
            .deliver_next(
                &tenant_scope(),
                connector_config.integration_id(),
                now + 2,
                integration_lease('L'),
                now + 3,
                &mut connector,
            )
            .expect("deliver live Jira issue update")
            .expect("due live Jira issue update"),
        OutboundAttemptResult::Delivered(_)
    ));
    let updated_issue = client.require_json(
        &format!("rest/api/3/issue/{issue_key}?fields=summary"),
        &[200],
    );
    assert_eq!(
        updated_issue
            .pointer("/fields/summary")
            .and_then(Value::as_str),
        Some(issue_summary.as_str())
    );

    let comment_create = outbound_request(
        &connector_config,
        &format!("jira-live-comment-{now}"),
        "jira.comment.create.v1",
        &json!({
            "body": live_adf("WinWinCode Jira live comment"),
            "issue_key": issue_key
        }),
    );
    framework
        .enqueue_outbound(&comment_create)
        .expect("enqueue live Jira comment");
    let comment_claim = framework
        .storage_mut()
        .claim_due(
            &tenant_scope(),
            connector_config.integration_id(),
            now + 3,
            integration_lease('M'),
            now + 4,
        )
        .expect("claim live Jira comment")
        .expect("due live Jira comment");
    assert!(
        winwincode_integration::ConnectorPort::deliver_outbound(&mut connector, &comment_claim)
            .expect("create live Jira comment")
            .remote_write_performed()
    );
    let comment_ids = client.comment_ids_for_marker(
        &issue_key,
        comment_create.operation_key().digest().0.as_str(),
    );
    assert_eq!(
        comment_ids.len(),
        1,
        "live Jira comment marker must identify exactly one comment"
    );
    let comment_id = comment_ids[0].clone();
    let comment_property = client.require_json(
        &format!(
            "rest/api/3/issue/{issue_key}/comment/{comment_id}/properties/winwincode.operation"
        ),
        &[200],
    );
    assert_eq!(
        comment_property
            .pointer("/value/key")
            .and_then(Value::as_str),
        Some(comment_create.operation_key().digest().0.as_str())
    );
    drop(framework);

    let mut framework =
        IntegrationFramework::new(IntegrationStorage::open(&root).expect("restart comment store"));
    let recovered_comment = framework
        .storage_mut()
        .claim_due(
            &tenant_scope(),
            connector_config.integration_id(),
            now + 4,
            integration_lease('N'),
            now + 5,
        )
        .expect("reclaim live Jira comment")
        .expect("abandoned live Jira comment");
    let comment_receipt =
        winwincode_integration::ConnectorPort::deliver_outbound(&mut connector, &recovered_comment)
            .expect("recover live Jira comment marker");
    assert!(!comment_receipt.remote_write_performed());
    framework
        .storage_mut()
        .record_success(
            &tenant_scope(),
            &recovered_comment,
            &comment_receipt,
            now + 4,
        )
        .expect("record live Jira comment recovery");
    assert_eq!(
        client
            .comment_ids_for_marker(
                &issue_key,
                comment_create.operation_key().digest().0.as_str()
            )
            .len(),
        1,
        "lost-response recovery must not create a second Jira comment"
    );

    let updated_comment_text = format!("WinWinCode Jira live comment updated {now}");
    let comment_update = outbound_request(
        &connector_config,
        &format!("jira-live-comment-update-{now}"),
        "jira.comment.update.v1",
        &json!({
            "body": live_adf(&updated_comment_text),
            "comment_id": comment_id,
            "issue_key": issue_key
        }),
    );
    framework
        .enqueue_outbound(&comment_update)
        .expect("enqueue live Jira comment update");
    assert!(matches!(
        framework
            .deliver_next(
                &tenant_scope(),
                connector_config.integration_id(),
                now + 5,
                integration_lease('O'),
                now + 6,
                &mut connector,
            )
            .expect("deliver live Jira comment update")
            .expect("due live Jira comment update"),
        OutboundAttemptResult::Delivered(_)
    ));
    let updated_comment = client.require_json(
        &format!("rest/api/3/issue/{issue_key}/comment/{comment_id}"),
        &[200],
    );
    assert!(
        serde_json::to_string(&updated_comment)
            .expect("updated live Jira comment JSON")
            .contains(&updated_comment_text)
    );

    let cross_project = outbound_request(
        &connector_config,
        &format!("jira-live-cross-project-{now}"),
        "jira.comment.create.v1",
        &json!({
            "body": live_adf("must not leave the configured Jira project"),
            "issue_key": "FOREIGN-1"
        }),
    );
    framework
        .enqueue_outbound(&cross_project)
        .expect("enqueue cross-project proof");
    assert!(matches!(
        framework
            .deliver_next(
                &tenant_scope(),
                connector_config.integration_id(),
                now + 6,
                integration_lease('P'),
                now + 7,
                &mut connector,
            )
            .expect("deliver cross-project proof")
            .expect("due cross-project proof"),
        OutboundAttemptResult::DeadLettered(_)
    ));

    let evidence = gate
        .webhook_evidence
        .as_ref()
        .expect("required live Jira webhook evidence");
    let revoked_probe = exercise_live_webhook_evidence(
        &mut framework,
        &connector_config,
        &credentials,
        evidence,
        now + 10_000,
    );

    let comment_delete = client.request(
        "DELETE",
        &format!("rest/api/3/issue/{issue_key}/comment/{comment_id}"),
        None,
    );
    assert_eq!(comment_delete.status, 204, "delete live Jira comment");
    assert_eq!(
        client
            .request(
                "GET",
                &format!("rest/api/3/issue/{issue_key}/comment/{comment_id}"),
                None,
            )
            .status,
        404,
        "deleted live Jira comment must be absent"
    );
    let issue_delete = client.request("DELETE", &format!("rest/api/3/issue/{issue_key}"), None);
    assert_eq!(issue_delete.status, 204, "delete live Jira issue");
    assert_eq!(
        client
            .request("GET", &format!("rest/api/3/issue/{issue_key}"), None)
            .status,
        404,
        "deleted live Jira issue must be absent"
    );
    cleanup.disarm();

    let authority = framework
        .storage()
        .authority(&tenant_scope(), connector_config.integration_id())
        .expect("live Jira authority");
    framework
        .revoke_credential(
            &tenant_scope(),
            connector_config.integration_id(),
            authority.revision(),
            now + 20_000,
        )
        .expect("revoke live Jira authority");
    credentials.revoke();
    let mut verifier = JiraWebhookVerifier::new(connector_config.clone(), credentials.clone());
    let mut inbound_connector = JiraEnterpriseConnector::try_new(
        connector_config.clone(),
        credentials.clone(),
        MapperFixture::default(),
        FixedClock(now),
    )
    .expect("revoked live Jira inbound connector");
    assert_eq!(
        framework
            .receive_webhook(&revoked_probe, &mut verifier, &mut inbound_connector)
            .expect_err("revoked live Jira inbound must fail")
            .kind(),
        IntegrationErrorKind::CredentialRevoked
    );
    let revoked_outbound = outbound_request(
        &connector_config,
        &format!("jira-live-revoked-{now}"),
        "jira.issue.create.v1",
        &json!({
            "description": null,
            "issue_type": "Task",
            "summary": "must not run after Jira credential revocation"
        }),
    );
    assert_eq!(
        framework
            .enqueue_outbound(&revoked_outbound)
            .expect_err("revoked live Jira outbound must fail")
            .kind(),
        IntegrationErrorKind::CredentialRevoked
    );

    drop(inbound_connector);
    drop(verifier);
    drop(connector);
    drop(framework);
    assert_directory_omits(&root, &gate.token);
    assert_directory_omits(&root, &gate.webhook_secret);
    for capture in &evidence.captures {
        assert_directory_omits(&root, capture.raw_body.as_bytes());
        assert_directory_omits(&root, capture.signature.as_bytes());
    }
    fs::remove_dir_all(root).expect("remove live Jira gate storage");
}

#[test]
#[ignore = "requires an explicitly revoked Jira Cloud OAuth grant"]
#[allow(clippy::too_many_lines)]
fn live_jira_cloud_revoked_oauth_is_immediately_dead_lettered_and_secret_safe() {
    let gate = LiveJiraGateConfig::load(LIVE_REVOKED_GATE_ENV, false);
    let now = current_time_millis();
    let client = LiveJiraClient::new(&gate);
    assert_eq!(
        client.accessible_resources().status,
        401,
        "the revoked Jira OAuth token must be rejected by Atlassian"
    );
    let connector_config = JiraConnectorConfig::try_new(
        integration_id(),
        credential_reference_id(),
        gate.site_id.clone(),
        gate.project_key.clone(),
        format!(
            "https://api.atlassian.com/ex/jira/{}",
            gate.site_id.as_str()
        ),
        JiraTlsRoots::WebPki,
    )
    .expect("revoked live Jira connector configuration");
    let credentials = LiveCredentialFixture::new(&gate, now + 30 * 60 * 1_000);
    let mut connector = JiraEnterpriseConnector::try_new(
        connector_config.clone(),
        credentials.clone(),
        MapperFixture::default(),
        FixedClock(now),
    )
    .expect("revoked live Jira connector");
    let root = temporary_directory("live-revoked");
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &connector_config);
    let request = outbound_request(
        &connector_config,
        &format!("jira-live-provider-revoked-{now}"),
        "jira.issue.create.v1",
        &json!({
            "description": null,
            "issue_type": gate.issue_type.clone(),
            "summary": "revoked Jira OAuth proof"
        }),
    );
    framework
        .enqueue_outbound(&request)
        .expect("enqueue revoked Jira proof");
    assert!(matches!(
        framework
            .deliver_next(
                &tenant_scope(),
                connector_config.integration_id(),
                now,
                integration_lease('Q'),
                now + 1,
                &mut connector,
            )
            .expect("deliver revoked Jira proof")
            .expect("due revoked Jira proof"),
        OutboundAttemptResult::DeadLettered(_)
    ));
    assert_eq!(
        framework
            .storage()
            .authority(&tenant_scope(), connector_config.integration_id())
            .expect("revoked Jira authority")
            .state(),
        ConnectorState::CredentialRevoked
    );
    let payload = serde_json::to_vec(&json!({
        "issue": {
            "fields": {"project": {"key": gate.project_key.as_str()}},
            "id": "revoked-issue",
            "key": format!("{}-1", gate.project_key.as_str())
        },
        "timestamp": now,
        "webhookEvent": "jira:issue_updated"
    }))
    .expect("revoked live webhook JSON");
    let inbound = JiraWebhookRequestFactory::new(connector_config.clone())
        .build(
            tenant_scope(),
            JiraWebhookHeaders::try_new(
                "revoked-live-webhook",
                b"sha256=0000000000000000000000000000000000000000000000000000000000000000",
            )
            .expect("revoked live webhook headers"),
            payload,
            now + 1,
        )
        .expect("revoked live webhook request");
    let mut verifier = JiraWebhookVerifier::new(connector_config.clone(), credentials.clone());
    let mut inbound_connector = JiraEnterpriseConnector::try_new(
        connector_config,
        credentials,
        MapperFixture::default(),
        FixedClock(now),
    )
    .expect("revoked live inbound connector");
    assert_eq!(
        framework
            .receive_webhook(&inbound, &mut verifier, &mut inbound_connector)
            .expect_err("revoked OAuth must also close inbound")
            .kind(),
        IntegrationErrorKind::CredentialRevoked
    );
    drop(inbound_connector);
    drop(verifier);
    drop(connector);
    drop(framework);
    assert_directory_omits(&root, &gate.token);
    assert_directory_omits(&root, &gate.webhook_secret);
    fs::remove_dir_all(root).expect("remove revoked live Jira gate storage");
}
