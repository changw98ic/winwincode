// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use winwincode_audit::AuditScope;
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, ProjectId, RepositoryId, WorkspaceId,
};
use winwincode_integration::{
    ConnectorCallError, ConnectorCallErrorKind, ConnectorProtocol, ConnectorRegistration,
    CredentialWebhookSignaturePort, EnterpriseIntegrationId, GenericWebhookConnector,
    GenericWebhookVerifier, InboundStatus, InboundWebhookMetadata, IntegrationErrorKind,
    IntegrationFramework, IntegrationLeaseId, IntegrationOperationKey, IntegrationStorage,
    OutboundAttemptResult, OutboundOperationState, OutboundRequest, RetryPolicy,
    WEBHOOK_CONNECTOR_PROTOCOL, WebhookAddressResolverPort, WebhookAuthenticationMode,
    WebhookClock, WebhookConnectorConfig, WebhookCredentialError, WebhookCredentialPort,
    WebhookEndpoint, WebhookHmacSecret, WebhookHttpPort, WebhookHttpRequest, WebhookHttpResponse,
    WebhookInboundPolicy, WebhookInboundProof, WebhookLimits, WebhookMappingField,
    WebhookMappingTemplate, WebhookOutboundAuthentication, WebhookRequestFactory,
    WebhookSignaturePort,
};

const HMAC_SECRET: &[u8; 32] = b"generic-webhook-hmac-secret-0001";
const APPROVED_CERTIFICATE: [u8; 32] = [0x5a; 32];
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-webhook-{name}-{}-{sequence}",
        std::process::id()
    ))
}

fn id(prefix: &str, tail: char) -> String {
    format!("{prefix}_{}", tail.to_string().repeat(26))
}

fn integration_id() -> EnterpriseIntegrationId {
    EnterpriseIntegrationId(id("int", 'A'))
}

fn credential_reference_id() -> CredentialReferenceId {
    CredentialReferenceId(id("crd", 'B'))
}

fn tenant_scope(organization: char, repository: char) -> AuditScope {
    AuditScope::repository(
        OrganizationId(id("org", organization)),
        WorkspaceId(id("wsp", 'C')),
        ProjectId(id("prj", 'D')),
        RepositoryId(id("rep", repository)),
    )
    .expect("tenant scope")
}

fn mapping_template() -> WebhookMappingTemplate {
    WebhookMappingTemplate::try_new(
        1,
        "fixture.event",
        "delivery.create",
        vec![
            WebhookMappingField::required("externalId", "/resource/id")
                .expect("external id mapping"),
            WebhookMappingField::required("action", "/action").expect("action mapping"),
            WebhookMappingField::optional("note", "/note").expect("note mapping"),
        ],
    )
    .expect("mapping template")
}

fn config(
    authentication: WebhookAuthenticationMode,
    limits: WebhookLimits,
) -> WebhookConnectorConfig {
    WebhookConnectorConfig::try_new(
        integration_id(),
        WebhookEndpoint::try_new(
            "https://hooks.example.com/v1/events",
            ["hooks.example.com".to_owned()],
        )
        .expect("endpoint"),
        WebhookInboundPolicy::try_new(authentication, 100, 10).expect("inbound policy"),
        vec![mapping_template()],
        limits,
    )
    .expect("webhook config")
}

fn registration(scope: AuditScope) -> ConnectorRegistration {
    ConnectorRegistration::try_new(
        integration_id(),
        scope,
        ConnectorProtocol::try_new(WEBHOOK_CONNECTOR_PROTOCOL).expect("protocol"),
        credential_reference_id(),
        10,
    )
    .expect("registration")
}

fn metadata(event_id: &str, sequence: u64, received_at: u64) -> InboundWebhookMetadata {
    InboundWebhookMetadata::try_new(
        "fixture.event",
        event_id,
        "fixture-resource",
        sequence,
        received_at,
    )
    .expect("metadata")
}

fn outbound_request(
    scope: AuditScope,
    identity: &str,
    payload: &[u8],
    retry_policy: RetryPolicy,
) -> OutboundRequest {
    OutboundRequest::try_new(
        integration_id(),
        scope,
        IntegrationOperationKey::derive(identity).expect("operation key"),
        "webhook.deliver",
        payload.to_vec(),
        retry_policy,
        100,
    )
    .expect("outbound request")
}

fn lease(tail: char) -> IntegrationLeaseId {
    IntegrationLeaseId::try_new(id("igl", tail)).expect("lease")
}

#[derive(Clone, Copy)]
struct FixedClock(u64);

impl WebhookClock for FixedClock {
    fn now_millis(&self) -> u64 {
        self.0
    }
}

#[derive(Clone, Default)]
struct CredentialFixture {
    state: Arc<Mutex<CredentialState>>,
}

#[derive(Default)]
struct CredentialState {
    hmac_lookups: usize,
    mtls_lookups: usize,
    revoked: bool,
}

impl CredentialFixture {
    fn lookups(&self) -> (usize, usize) {
        let state = self.state.lock().expect("credential state");
        (state.hmac_lookups, state.mtls_lookups)
    }
}

impl WebhookCredentialPort for CredentialFixture {
    fn resolve_hmac_secret(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<WebhookHmacSecret, WebhookCredentialError> {
        assert_eq!(reference, &credential_reference_id());
        let mut state = self.state.lock().expect("credential state");
        state.hmac_lookups += 1;
        if state.revoked {
            return Err(WebhookCredentialError::revoked());
        }
        WebhookHmacSecret::try_new(HMAC_SECRET.to_vec())
            .map_err(|_| WebhookCredentialError::rejected())
    }

    fn authorize_mtls_peer(
        &mut self,
        reference: &CredentialReferenceId,
        peer_certificate_sha256: &[u8; 32],
    ) -> Result<(), WebhookCredentialError> {
        assert_eq!(reference, &credential_reference_id());
        let mut state = self.state.lock().expect("credential state");
        state.mtls_lookups += 1;
        if state.revoked {
            return Err(WebhookCredentialError::revoked());
        }
        if peer_certificate_sha256 == &APPROVED_CERTIFICATE {
            Ok(())
        } else {
            Err(WebhookCredentialError::rejected())
        }
    }
}

#[derive(Clone)]
struct ResolverFixture {
    addresses: Vec<IpAddr>,
    calls: Arc<AtomicU64>,
}

impl ResolverFixture {
    fn public() -> Self {
        Self {
            addresses: vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
            calls: Arc::new(AtomicU64::new(0)),
        }
    }

    fn private() -> Self {
        Self {
            addresses: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            calls: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl WebhookAddressResolverPort for ResolverFixture {
    fn resolve(&mut self, host: &str, port: u16) -> Result<Vec<IpAddr>, ConnectorCallError> {
        assert_eq!(host, "hooks.example.com");
        assert_eq!(port, 443);
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(self.addresses.clone())
    }
}

#[derive(Clone, Default)]
struct TransportFixture {
    state: Arc<Mutex<TransportState>>,
}

#[derive(Default)]
struct TransportState {
    requests: Vec<WebhookHttpRequest>,
    responses: VecDeque<Result<WebhookHttpResponse, ConnectorCallError>>,
}

impl TransportFixture {
    fn push(&self, response: WebhookHttpResponse) {
        self.state
            .lock()
            .expect("transport state")
            .responses
            .push_back(Ok(response));
    }

    fn requests(&self) -> Vec<WebhookHttpRequest> {
        self.state.lock().expect("transport state").requests.clone()
    }
}

impl WebhookHttpPort for TransportFixture {
    fn send(
        &mut self,
        request: &WebhookHttpRequest,
    ) -> Result<WebhookHttpResponse, ConnectorCallError> {
        let mut state = self.state.lock().expect("transport state");
        state.requests.push(request.clone());
        state.responses.pop_front().unwrap_or_else(|| {
            Err(
                ConnectorCallError::try_new(ConnectorCallErrorKind::Retryable, "FIXTURE_EMPTY")
                    .expect("fixture error"),
            )
        })
    }
}

fn connector(
    config: WebhookConnectorConfig,
    resolver: ResolverFixture,
    transport: TransportFixture,
    credentials: CredentialFixture,
    now: u64,
) -> GenericWebhookConnector<
    ResolverFixture,
    TransportFixture,
    CredentialWebhookSignaturePort<CredentialFixture>,
    FixedClock,
> {
    GenericWebhookConnector::new(
        config,
        resolver,
        transport,
        CredentialWebhookSignaturePort::new(credentials),
        FixedClock(now),
    )
}

fn hmac_request(
    config: &WebhookConnectorConfig,
    credentials: CredentialFixture,
    scope: AuditScope,
    event_id: &str,
    sequence: u64,
    signed_at: u64,
    payload: Vec<u8>,
) -> winwincode_integration::InboundWebhookRequest {
    let signature = CredentialWebhookSignaturePort::new(credentials)
        .sign_hmac_sha256(&credential_reference_id(), signed_at, &payload)
        .expect("sign webhook");
    WebhookRequestFactory::new(config)
        .build(
            scope,
            &metadata(event_id, sequence, signed_at),
            &WebhookInboundProof::hmac_sha256(signed_at, signature).expect("HMAC proof"),
            payload,
        )
        .expect("webhook request")
}

fn assert_exact_hmac_requests(requests: &[WebhookHttpRequest]) {
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].operation_key(), requests[1].operation_key());
    assert_eq!(requests[0].body(), requests[1].body());
    assert_eq!(requests[0].addresses(), requests[1].addresses());
    assert_eq!(requests[0].endpoint().host(), "hooks.example.com");
    assert_eq!(requests[0].timeout_millis(), 30_000);
    assert!(matches!(
        requests[0].authentication(),
        WebhookOutboundAuthentication::HmacSha256 { .. }
    ));
    let envelope: serde_json::Value =
        serde_json::from_slice(requests[0].body()).expect("outbound envelope");
    assert_eq!(
        envelope.get("schema").and_then(serde_json::Value::as_str),
        Some("winwincode.webhook.delivery.v1")
    );
    assert_eq!(
        envelope
            .get("eventType")
            .and_then(serde_json::Value::as_str),
        Some("webhook.deliver")
    );
    assert_eq!(envelope["payload"]["action"], "sync");
    assert!(
        envelope["idempotencyKey"]
            .as_str()
            .is_some_and(|value| value.starts_with("sha256:"))
    );
}

fn assert_single_canonical_dispatch(framework: &IntegrationFramework, tenant: &AuditScope) {
    let dispatches = framework
        .storage()
        .inbound_dispatches(tenant, &integration_id(), 0, 10)
        .expect("dispatches");
    assert_eq!(dispatches.len(), 1);
    assert_eq!(dispatches[0].command_name(), "delivery.create");
    assert_eq!(
        dispatches[0].command_payload(),
        br#"{"action":"opened","externalId":42}"#
    );
}

#[test]
fn canonical_hmac_mapping_replays_orders_isolates_scope_and_survives_restart() {
    let root = temporary_directory("hmac-inbound");
    let limits = WebhookLimits::try_new(128, 128, 128, 1_000).expect("limits");
    let config = config(WebhookAuthenticationMode::HmacSha256, limits);
    let tenant = tenant_scope('E', 'F');
    let credentials = CredentialFixture::default();
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    framework
        .register_connector(&registration(tenant.clone()))
        .expect("register");
    let mut verifier = GenericWebhookVerifier::new(
        &config,
        CredentialWebhookSignaturePort::new(credentials.clone()),
        FixedClock(1_000),
    );
    let mut connector = connector(
        config.clone(),
        ResolverFixture::public(),
        TransportFixture::default(),
        credentials.clone(),
        1_000,
    );
    let payload = br#"{"resource":{"id":42},"action":"opened"}"#.to_vec();
    let request = hmac_request(
        &config,
        credentials.clone(),
        tenant.clone(),
        "provider-event-a",
        2,
        1_000,
        payload.clone(),
    );
    let accepted = framework
        .receive_webhook(&request, &mut verifier, &mut connector)
        .expect("accepted webhook");
    assert_eq!(accepted.status(), InboundStatus::Accepted);
    assert!(!accepted.idempotent_replay());
    let replay_with_changed_unsigned_id = hmac_request(
        &config,
        credentials.clone(),
        tenant.clone(),
        "provider-event-unsigned-alias",
        2,
        1_000,
        payload.clone(),
    );
    assert!(
        framework
            .receive_webhook(
                &replay_with_changed_unsigned_id,
                &mut verifier,
                &mut connector,
            )
            .expect("authenticated exact replay")
            .idempotent_replay()
    );
    let older = hmac_request(
        &config,
        credentials.clone(),
        tenant.clone(),
        "provider-event-older",
        1,
        999,
        payload.clone(),
    );
    assert_eq!(
        framework
            .receive_webhook(&older, &mut verifier, &mut connector)
            .expect("older webhook")
            .status(),
        InboundStatus::IgnoredOutOfOrder
    );
    assert_single_canonical_dispatch(&framework, &tenant);

    drop(framework);
    let mut restarted =
        IntegrationFramework::new(IntegrationStorage::open(&root).expect("restart store"));
    assert!(
        restarted
            .receive_webhook(&request, &mut verifier, &mut connector)
            .expect("restart replay")
            .idempotent_replay()
    );
    assert_eq!(
        restarted
            .storage()
            .inbound_dispatches(&tenant, &integration_id(), 0, 10)
            .expect("restart dispatches")
            .len(),
        1
    );
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn inbound_scope_time_mapping_and_size_fail_before_dispatch() {
    let root = temporary_directory("inbound-boundaries");
    let limits = WebhookLimits::try_new(128, 128, 128, 1_000).expect("limits");
    let config = config(WebhookAuthenticationMode::HmacSha256, limits);
    let tenant = tenant_scope('E', 'F');
    let credentials = CredentialFixture::default();
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    framework
        .register_connector(&registration(tenant.clone()))
        .expect("register");
    let mut verifier = GenericWebhookVerifier::new(
        &config,
        CredentialWebhookSignaturePort::new(credentials.clone()),
        FixedClock(1_000),
    );
    let mut connector = connector(
        config.clone(),
        ResolverFixture::public(),
        TransportFixture::default(),
        credentials.clone(),
        1_000,
    );
    let payload = br#"{"resource":{"id":42},"action":"opened"}"#.to_vec();
    let foreign_request = hmac_request(
        &config,
        credentials.clone(),
        tenant_scope('G', 'H'),
        "provider-event-foreign",
        1,
        1_000,
        payload.clone(),
    );
    assert_eq!(
        framework
            .receive_webhook(&foreign_request, &mut verifier, &mut connector)
            .expect_err("foreign scope")
            .kind(),
        IntegrationErrorKind::TenantMismatch
    );
    let stale = hmac_request(
        &config,
        credentials.clone(),
        tenant.clone(),
        "provider-event-stale",
        1,
        800,
        payload,
    );
    assert_eq!(
        framework
            .receive_webhook(&stale, &mut verifier, &mut connector)
            .expect_err("stale signature")
            .kind(),
        IntegrationErrorKind::SignatureRejected
    );
    let missing_required = hmac_request(
        &config,
        credentials,
        tenant.clone(),
        "provider-event-missing",
        1,
        1_000,
        br#"{"action":"opened"}"#.to_vec(),
    );
    assert_eq!(
        framework
            .receive_webhook(&missing_required, &mut verifier, &mut connector)
            .expect_err("required mapping field")
            .kind(),
        IntegrationErrorKind::ConnectorRejected
    );
    assert_eq!(
        WebhookRequestFactory::new(&config)
            .build(
                tenant.clone(),
                &metadata("provider-event-large", 2, 1_000),
                &WebhookInboundProof::hmac_sha256(1_000, [1; 32]).expect("proof"),
                vec![b'x'; limits.max_inbound_body_bytes() + 1],
            )
            .expect_err("oversized body")
            .kind(),
        IntegrationErrorKind::Invalid
    );
    assert!(
        framework
            .storage()
            .inbound_dispatches(&tenant, &integration_id(), 0, 10)
            .expect("dispatches")
            .is_empty()
    );
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn mutual_tls_proof_is_windowed_and_exactly_authorized() {
    let root = temporary_directory("mtls-inbound");
    let config = config(
        WebhookAuthenticationMode::MutualTls,
        WebhookLimits::default(),
    );
    let tenant = tenant_scope('J', 'K');
    let credentials = CredentialFixture::default();
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    framework
        .register_connector(&registration(tenant.clone()))
        .expect("register");
    let mut verifier = GenericWebhookVerifier::new(
        &config,
        CredentialWebhookSignaturePort::new(credentials.clone()),
        FixedClock(2_000),
    );
    let mut connector = connector(
        config.clone(),
        ResolverFixture::public(),
        TransportFixture::default(),
        credentials.clone(),
        2_000,
    );
    let factory = WebhookRequestFactory::new(&config);
    let payload = br#"{"action":"updated","resource":{"id":7}}"#.to_vec();
    let accepted = factory
        .build(
            tenant.clone(),
            &metadata("mtls-event-a", 1, 2_000),
            &WebhookInboundProof::mutual_tls(2_000, APPROVED_CERTIFICATE).expect("mTLS proof"),
            payload.clone(),
        )
        .expect("mTLS request");
    assert_eq!(
        framework
            .receive_webhook(&accepted, &mut verifier, &mut connector)
            .expect("accepted mTLS webhook")
            .status(),
        InboundStatus::Accepted
    );
    let rejected = factory
        .build(
            tenant.clone(),
            &metadata("mtls-event-b", 2, 2_000),
            &WebhookInboundProof::mutual_tls(2_000, [0x33; 32]).expect("mTLS proof"),
            payload.clone(),
        )
        .expect("rejected mTLS request");
    assert_eq!(
        framework
            .receive_webhook(&rejected, &mut verifier, &mut connector)
            .expect_err("untrusted peer")
            .kind(),
        IntegrationErrorKind::SignatureRejected
    );
    let mut signing = CredentialWebhookSignaturePort::new(credentials.clone());
    let hmac = signing
        .sign_hmac_sha256(&credential_reference_id(), 2_000, &payload)
        .expect("HMAC");
    assert_eq!(
        factory
            .build(
                tenant,
                &metadata("wrong-mode", 3, 2_000),
                &WebhookInboundProof::hmac_sha256(2_000, hmac).expect("HMAC proof"),
                payload,
            )
            .expect_err("wrong authentication mode")
            .kind(),
        IntegrationErrorKind::Invalid
    );
    assert_eq!(credentials.lookups().1, 2);
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn outbound_retry_reuses_exact_operation_and_pinned_public_destination() {
    let root = temporary_directory("outbound-retry");
    let config = config(
        WebhookAuthenticationMode::HmacSha256,
        WebhookLimits::default(),
    );
    let tenant = tenant_scope('M', 'N');
    let credentials = CredentialFixture::default();
    let transport = TransportFixture::default();
    transport.push(
        WebhookHttpResponse::try_new(503, b"temporarily unavailable".to_vec(), Some(25))
            .expect("retry response"),
    );
    transport.push(
        WebhookHttpResponse::try_new(201, b"created".to_vec(), None).expect("success response"),
    );
    let resolver = ResolverFixture::public();
    let resolver_calls = Arc::clone(&resolver.calls);
    let mut adapter = connector(
        config,
        resolver,
        transport.clone(),
        credentials.clone(),
        100,
    );
    let request = outbound_request(
        tenant.clone(),
        "outbound-exact-operation",
        br#"{"action":"sync"}"#,
        RetryPolicy::try_new(3, 10, 40).expect("retry policy"),
    );
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    framework
        .register_connector(&registration(tenant.clone()))
        .expect("register");
    assert!(
        !framework
            .enqueue_outbound(&request)
            .expect("enqueue")
            .idempotent_replay()
    );
    assert!(
        framework
            .enqueue_outbound(&request)
            .expect("enqueue replay")
            .idempotent_replay()
    );
    let first = framework
        .deliver_next(
            &tenant,
            &integration_id(),
            100,
            lease('P'),
            110,
            &mut adapter,
        )
        .expect("first attempt")
        .expect("first due operation");
    let OutboundAttemptResult::RetryScheduled(retry) = first else {
        panic!("expected retry");
    };
    assert_eq!(retry.eligible_at_millis(), 125);
    drop(framework);

    let mut restarted =
        IntegrationFramework::new(IntegrationStorage::open(&root).expect("restart store"));
    let second = restarted
        .deliver_next(
            &tenant,
            &integration_id(),
            125,
            lease('Q'),
            135,
            &mut adapter,
        )
        .expect("second attempt")
        .expect("retry due");
    let OutboundAttemptResult::Delivered(delivered) = second else {
        panic!("expected delivery");
    };
    assert_eq!(
        delivered.operation().state(),
        OutboundOperationState::Delivered
    );
    assert_eq!(delivered.operation().attempt(), 2);
    let requests = transport.requests();
    assert_exact_hmac_requests(&requests);
    assert_eq!(resolver_calls.load(Ordering::Relaxed), 2);
    assert_eq!(credentials.lookups().0, 2);
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn endpoint_ssrf_and_response_size_fail_closed_into_dead_letter() {
    for (endpoint, allowed) in [
        ("http://hooks.example.com/events", "hooks.example.com"),
        ("https://user@hooks.example.com/events", "hooks.example.com"),
        ("https://127.0.0.1/events", "127.0.0.1"),
        ("https://hooks.example.com/events", "foreign.example.com"),
    ] {
        assert_eq!(
            WebhookEndpoint::try_new(endpoint, [allowed.to_owned()])
                .expect_err("blocked endpoint")
                .kind(),
            IntegrationErrorKind::Invalid
        );
    }

    let root = temporary_directory("ssrf");
    let limits = WebhookLimits::try_new(128, 512, 8, 1_000).expect("limits");
    let config = config(WebhookAuthenticationMode::MutualTls, limits);
    let tenant = tenant_scope('R', 'S');
    let credentials = CredentialFixture::default();
    let transport = TransportFixture::default();
    let mut blocked_adapter = connector(
        config.clone(),
        ResolverFixture::private(),
        transport.clone(),
        credentials.clone(),
        100,
    );
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    framework
        .register_connector(&registration(tenant.clone()))
        .expect("register");
    framework
        .enqueue_outbound(&outbound_request(
            tenant.clone(),
            "private-destination",
            br#"{"action":"sync"}"#,
            RetryPolicy::try_new(3, 5, 10).expect("retry policy"),
        ))
        .expect("enqueue blocked call");
    let blocked = framework
        .deliver_next(
            &tenant,
            &integration_id(),
            100,
            lease('T'),
            110,
            &mut blocked_adapter,
        )
        .expect("blocked attempt")
        .expect("blocked operation");
    assert!(matches!(blocked, OutboundAttemptResult::DeadLettered(_)));
    assert!(transport.requests().is_empty());

    transport
        .push(WebhookHttpResponse::try_new(200, vec![b'x'; 9], None).expect("oversized response"));
    let mut response_adapter = connector(
        config,
        ResolverFixture::public(),
        transport.clone(),
        credentials,
        101,
    );
    framework
        .enqueue_outbound(&outbound_request(
            tenant.clone(),
            "oversized-response",
            br#"{"action":"sync"}"#,
            RetryPolicy::try_new(3, 5, 10).expect("retry policy"),
        ))
        .expect("enqueue response call");
    let oversized = framework
        .deliver_next(
            &tenant,
            &integration_id(),
            101,
            lease('V'),
            111,
            &mut response_adapter,
        )
        .expect("oversized attempt")
        .expect("oversized operation");
    let OutboundAttemptResult::DeadLettered(receipt) = oversized else {
        panic!("expected oversized response dead letter");
    };
    assert_eq!(receipt.operation().attempt(), 1);
    assert_eq!(transport.requests().len(), 1);
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}
