// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use winwincode_audit::AuditScope;
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, ProjectId, RepositoryId, Sha256Digest, WorkspaceId,
};
use winwincode_integration::{
    ConnectorAuthority, ConnectorCallError, ConnectorCallErrorKind, ConnectorPort,
    ConnectorProtocol, ConnectorRegistration, ConnectorState, EnterpriseIntegrationId,
    InboundNormalizationContext, InboundStatus, InboundWebhookMetadata, InboundWebhookRequest,
    IntegrationAuditKind, IntegrationErrorKind, IntegrationFramework, IntegrationLeaseId,
    IntegrationOperationKey, IntegrationStorage, NormalizedInboundEvent, OutboundAttemptResult,
    OutboundCallReceipt, OutboundOperationState, OutboundRequest, RetryPolicy,
    SignatureVerificationError, WebhookSignatureVerifier,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-integration-{name}-{}-{sequence}",
        std::process::id()
    ))
}

fn id(prefix: &str, tail: char) -> String {
    format!("{prefix}_{}", tail.to_string().repeat(26))
}

fn digest(tail: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", tail.to_string().repeat(64)))
}

fn scope(organization: char, repository: char) -> AuditScope {
    AuditScope::repository(
        OrganizationId(id("org", organization)),
        WorkspaceId(id("wsp", '2')),
        ProjectId(id("prj", '3')),
        RepositoryId(id("rep", repository)),
    )
    .expect("scope")
}

fn integration_id(tail: char) -> EnterpriseIntegrationId {
    EnterpriseIntegrationId(id("int", tail))
}

fn registration(
    integration_id: EnterpriseIntegrationId,
    scope: AuditScope,
    credential: char,
) -> ConnectorRegistration {
    ConnectorRegistration::try_new(
        integration_id,
        scope,
        ConnectorProtocol::try_new("webhook.v1").expect("protocol"),
        CredentialReferenceId(id("crd", credential)),
        10,
    )
    .expect("registration")
}

fn inbound(
    integration_id: EnterpriseIntegrationId,
    scope: AuditScope,
    event_id: &str,
    sequence: u64,
    signature: &[u8],
    payload: &[u8],
    received_at: u64,
) -> InboundWebhookRequest {
    InboundWebhookRequest::try_new(
        integration_id,
        scope,
        InboundWebhookMetadata::try_new(
            "fixture.event",
            event_id,
            "fixture-stream",
            sequence,
            received_at,
        )
        .expect("webhook metadata"),
        signature.to_vec(),
        payload.to_vec(),
    )
    .expect("inbound request")
}

fn outbound(
    integration_id: EnterpriseIntegrationId,
    scope: AuditScope,
    operation_id: &str,
    retry_policy: RetryPolicy,
    enqueued_at: u64,
) -> OutboundRequest {
    OutboundRequest::try_new(
        integration_id,
        scope,
        IntegrationOperationKey::derive(operation_id).expect("operation key"),
        "issue.sync",
        br#"{"action":"sync"}"#.to_vec(),
        retry_policy,
        enqueued_at,
    )
    .expect("outbound request")
}

fn lease(tail: char) -> IntegrationLeaseId {
    IntegrationLeaseId::try_new(id("igl", tail)).expect("lease id")
}

#[derive(Default)]
struct VerifierFixture {
    calls: usize,
    credentials: Vec<CredentialReferenceId>,
    revoked: bool,
}

impl WebhookSignatureVerifier for VerifierFixture {
    fn verify(
        &mut self,
        authority: &ConnectorAuthority,
        signature: &[u8],
        _payload: &[u8],
    ) -> Result<(), SignatureVerificationError> {
        self.calls += 1;
        self.credentials
            .push(authority.credential_reference_id().clone());
        if self.revoked {
            return Err(SignatureVerificationError::credential_revoked());
        }
        if signature == b"valid-signature" {
            Ok(())
        } else {
            Err(SignatureVerificationError::rejected())
        }
    }
}

#[derive(Default)]
struct ConnectorFixture {
    normalize_calls: usize,
    delivery_calls: Vec<(IntegrationOperationKey, u32, Vec<u8>)>,
    outcomes: VecDeque<Result<OutboundCallReceipt, ConnectorCallError>>,
}

impl ConnectorPort for ConnectorFixture {
    fn normalize_inbound(
        &mut self,
        _authority: &ConnectorAuthority,
        _context: &InboundNormalizationContext,
        _payload: &[u8],
    ) -> Result<NormalizedInboundEvent, ConnectorCallError> {
        self.normalize_calls += 1;
        Ok(NormalizedInboundEvent::try_new(
            "delivery.create",
            br#"{"command":"delivery.create"}"#.to_vec(),
        )
        .expect("normalized command"))
    }

    fn deliver_outbound(
        &mut self,
        claim: &winwincode_integration::OutboundClaim,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        self.delivery_calls.push((
            claim.operation_key().clone(),
            claim.attempt(),
            claim.payload().to_vec(),
        ));
        self.outcomes
            .pop_front()
            .expect("configured connector outcome")
    }
}

fn connector_error(kind: ConnectorCallErrorKind, code: &str) -> ConnectorCallError {
    ConnectorCallError::try_new(kind, code).expect("connector error")
}

fn assert_inbound_audit_is_secret_safe(
    framework: &IntegrationFramework,
    tenant: &AuditScope,
    integration_id: &EnterpriseIntegrationId,
) {
    let audit = framework
        .storage()
        .audit_facts(tenant, integration_id, 0, 20)
        .expect("audit facts");
    assert_eq!(
        audit
            .iter()
            .map(winwincode_integration::IntegrationAuditFact::kind)
            .collect::<Vec<_>>(),
        vec![
            IntegrationAuditKind::ConnectorRegistered,
            IntegrationAuditKind::InboundAccepted,
            IntegrationAuditKind::InboundIgnored,
        ]
    );
    let audit_json = serde_json::to_string(&audit).expect("audit json");
    for secret in [
        "valid-signature",
        "top-secret-payload",
        "another-secret",
        "provider-event-top-secret-id",
    ] {
        assert!(!audit_json.contains(secret));
    }
}

fn assert_outbound_retry_audit(
    framework: &IntegrationFramework,
    tenant: &AuditScope,
    integration_id: &EnterpriseIntegrationId,
) {
    let audit = framework
        .storage()
        .audit_facts(tenant, integration_id, 0, 20)
        .expect("audit");
    assert_eq!(
        audit
            .iter()
            .map(winwincode_integration::IntegrationAuditFact::kind)
            .collect::<Vec<_>>(),
        vec![
            IntegrationAuditKind::ConnectorRegistered,
            IntegrationAuditKind::OutboundEnqueued,
            IntegrationAuditKind::OutboundRetryScheduled,
            IntegrationAuditKind::OutboundDelivered,
        ]
    );
}

fn deliver_permanent_failure(
    framework: &mut IntegrationFramework,
    connector: &mut ConnectorFixture,
    tenant: &AuditScope,
    integration_id: &EnterpriseIntegrationId,
) -> OutboundRequest {
    let request = outbound(
        integration_id.clone(),
        tenant.clone(),
        "outbound-operation-permanent",
        RetryPolicy::try_new(3, 5, 20).expect("retry policy"),
        106,
    );
    framework
        .enqueue_outbound(&request)
        .expect("enqueue permanent operation");
    connector.outcomes.push_back(Err(connector_error(
        ConnectorCallErrorKind::Permanent,
        "REMOTE_REJECTED",
    )));
    let result = framework
        .deliver_next(tenant, integration_id, 106, lease('M'), 111, connector)
        .expect("permanent attempt")
        .expect("permanent result");
    let OutboundAttemptResult::DeadLettered(receipt) = result else {
        panic!("expected permanent dead letter");
    };
    assert_eq!(receipt.operation().attempt(), 1);
    request
}

#[test]
fn inbound_signature_replay_ordering_and_audit_are_secret_safe_across_restart() {
    let root = temporary_directory("inbound");
    let integration_id = integration_id('A');
    let tenant = scope('1', '4');
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    let registered = framework
        .register_connector(&registration(integration_id.clone(), tenant.clone(), '5'))
        .expect("register");
    assert!(!registered.idempotent_replay());
    assert_eq!(registered.authority().state(), ConnectorState::Active);

    let mut verifier = VerifierFixture::default();
    let mut connector = ConnectorFixture::default();
    let accepted_request = inbound(
        integration_id.clone(),
        tenant.clone(),
        "provider-event-top-secret-id",
        2,
        b"valid-signature",
        br#"{"token":"top-secret-payload"}"#,
        20,
    );
    let accepted = framework
        .receive_webhook(&accepted_request, &mut verifier, &mut connector)
        .expect("accepted webhook");
    assert_eq!(accepted.status(), InboundStatus::Accepted);
    assert!(!accepted.idempotent_replay());
    let replay = framework
        .receive_webhook(&accepted_request, &mut verifier, &mut connector)
        .expect("exact replay");
    assert_eq!(replay.status(), InboundStatus::Accepted);
    assert!(replay.idempotent_replay());

    let ignored = framework
        .receive_webhook(
            &inbound(
                integration_id.clone(),
                tenant.clone(),
                "older-provider-event",
                1,
                b"valid-signature",
                br#"{"token":"another-secret"}"#,
                21,
            ),
            &mut verifier,
            &mut connector,
        )
        .expect("out of order receipt");
    assert_eq!(ignored.status(), InboundStatus::IgnoredOutOfOrder);
    let dispatches = framework
        .storage()
        .inbound_dispatches(&tenant, &integration_id, 0, 10)
        .expect("dispatches");
    assert_eq!(dispatches.len(), 1);
    assert_eq!(dispatches[0].command_name(), "delivery.create");

    let changed = inbound(
        integration_id.clone(),
        tenant.clone(),
        "provider-event-top-secret-id",
        2,
        b"valid-signature",
        br#"{"token":"changed-secret"}"#,
        22,
    );
    assert_eq!(
        framework
            .receive_webhook(&changed, &mut verifier, &mut connector)
            .expect_err("changed event reuse")
            .kind(),
        IntegrationErrorKind::Conflict
    );
    assert_eq!(verifier.calls, 4);
    assert_eq!(connector.normalize_calls, 4);

    assert_inbound_audit_is_secret_safe(&framework, &tenant, &integration_id);

    drop(framework);
    let mut restarted =
        IntegrationFramework::new(IntegrationStorage::open(&root).expect("restart store"));
    let replay = restarted
        .receive_webhook(&accepted_request, &mut verifier, &mut connector)
        .expect("restart replay");
    assert!(replay.idempotent_replay());
    assert_eq!(
        restarted
            .storage()
            .inbound_dispatches(&tenant, &integration_id, 0, 10)
            .expect("restart dispatches")
            .len(),
        1
    );
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn invalid_signature_and_foreign_tenant_have_zero_dispatch_side_effects() {
    let root = temporary_directory("signature");
    let integration_id = integration_id('B');
    let tenant = scope('1', '4');
    let foreign = scope('6', '7');
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    framework
        .register_connector(&registration(integration_id.clone(), tenant.clone(), '5'))
        .expect("register");
    assert!(
        framework
            .register_connector(&registration(integration_id.clone(), tenant.clone(), '5'))
            .expect("registration replay")
            .idempotent_replay()
    );
    assert_eq!(
        framework
            .register_connector(&registration(integration_id.clone(), tenant.clone(), '6'))
            .expect_err("changed registration")
            .kind(),
        IntegrationErrorKind::Conflict
    );
    let mut verifier = VerifierFixture::default();
    let mut connector = ConnectorFixture::default();
    let bad = inbound(
        integration_id.clone(),
        tenant.clone(),
        "bad-signature-event",
        1,
        b"invalid-signature",
        br#"{"value":1}"#,
        20,
    );
    assert_eq!(
        framework
            .receive_webhook(&bad, &mut verifier, &mut connector)
            .expect_err("invalid signature")
            .kind(),
        IntegrationErrorKind::SignatureRejected
    );
    assert_eq!(verifier.calls, 1);
    assert_eq!(connector.normalize_calls, 0);

    let foreign_request = inbound(
        integration_id.clone(),
        foreign,
        "foreign-event",
        1,
        b"valid-signature",
        br#"{"value":1}"#,
        21,
    );
    assert_eq!(
        framework
            .receive_webhook(&foreign_request, &mut verifier, &mut connector)
            .expect_err("foreign tenant")
            .kind(),
        IntegrationErrorKind::TenantMismatch
    );
    assert_eq!(verifier.calls, 1);
    assert_eq!(
        framework
            .enqueue_outbound(&outbound(
                integration_id.clone(),
                scope('6', '7'),
                "foreign-operation",
                RetryPolicy::try_new(2, 5, 5).expect("retry policy"),
                22,
            ))
            .expect_err("foreign outbound")
            .kind(),
        IntegrationErrorKind::TenantMismatch
    );
    assert!(
        framework
            .storage()
            .inbound_dispatches(&tenant, &integration_id, 0, 10)
            .expect("dispatches")
            .is_empty()
    );
    assert_eq!(
        framework
            .storage()
            .audit_facts(&tenant, &integration_id, 0, 10)
            .expect("audit")
            .len(),
        1
    );
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn verifier_credential_revocation_durably_stops_later_webhook_calls() {
    let root = temporary_directory("verifier-revocation");
    let integration_id = integration_id('N');
    let tenant = scope('1', '4');
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    framework
        .register_connector(&registration(integration_id.clone(), tenant.clone(), '5'))
        .expect("register");
    let request = inbound(
        integration_id.clone(),
        tenant.clone(),
        "credential-revoked-event",
        1,
        b"valid-signature",
        br#"{"value":1}"#,
        20,
    );
    let mut revoked_verifier = VerifierFixture {
        revoked: true,
        ..VerifierFixture::default()
    };
    let mut connector = ConnectorFixture::default();
    assert_eq!(
        framework
            .receive_webhook(&request, &mut revoked_verifier, &mut connector)
            .expect_err("revoked credential")
            .kind(),
        IntegrationErrorKind::CredentialRevoked
    );
    assert_eq!(revoked_verifier.calls, 1);
    assert_eq!(connector.normalize_calls, 0);
    assert_eq!(
        framework
            .storage()
            .authority(&tenant, &integration_id)
            .expect("revoked authority")
            .state(),
        ConnectorState::CredentialRevoked
    );
    let mut later_verifier = VerifierFixture::default();
    assert_eq!(
        framework
            .receive_webhook(&request, &mut later_verifier, &mut connector)
            .expect_err("later webhook")
            .kind(),
        IntegrationErrorKind::CredentialRevoked
    );
    assert_eq!(later_verifier.calls, 0);
    assert!(
        framework
            .storage()
            .inbound_dispatches(&tenant, &integration_id, 0, 10)
            .expect("dispatches")
            .is_empty()
    );
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn concurrent_duplicate_webhooks_create_one_formal_command_dispatch() {
    let root = temporary_directory("concurrent-inbound");
    let integration_id = integration_id('M');
    let tenant = scope('1', '4');
    let mut bootstrap = IntegrationStorage::open(&root).expect("bootstrap store");
    bootstrap
        .register(&registration(integration_id.clone(), tenant.clone(), '5'))
        .expect("register");
    drop(bootstrap);

    let request = inbound(
        integration_id.clone(),
        tenant.clone(),
        "concurrent-provider-event",
        1,
        b"valid-signature",
        br#"{"value":1}"#,
        20,
    );
    let barrier = Arc::new(Barrier::new(2));
    let handles = (0..2)
        .map(|_| {
            let root = root.clone();
            let request = request.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut framework = IntegrationFramework::new(
                    IntegrationStorage::open(root).expect("concurrent store"),
                );
                let mut verifier = VerifierFixture::default();
                let mut connector = ConnectorFixture::default();
                barrier.wait();
                framework
                    .receive_webhook(&request, &mut verifier, &mut connector)
                    .expect("concurrent webhook")
            })
        })
        .collect::<Vec<_>>();
    let receipts = handles
        .into_iter()
        .map(|handle| handle.join().expect("webhook thread"))
        .collect::<Vec<_>>();
    assert!(
        receipts
            .iter()
            .all(|receipt| receipt.status() == InboundStatus::Accepted)
    );
    assert_eq!(
        receipts
            .iter()
            .filter(|receipt| receipt.idempotent_replay())
            .count(),
        1
    );
    let storage = IntegrationStorage::open(&root).expect("verify store");
    assert_eq!(
        storage
            .inbound_dispatches(&tenant, &integration_id, 0, 10)
            .expect("dispatches")
            .len(),
        1
    );
    assert_eq!(
        storage
            .audit_facts(&tenant, &integration_id, 0, 10)
            .expect("audit")
            .len(),
        2
    );
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn outbound_retry_uses_one_idempotency_key_and_delivers_after_capped_backoff() {
    let root = temporary_directory("retry");
    let integration_id = integration_id('C');
    let tenant = scope('1', '4');
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    framework
        .register_connector(&registration(integration_id.clone(), tenant.clone(), '5'))
        .expect("register");
    let request = outbound(
        integration_id.clone(),
        tenant.clone(),
        "outbound-operation-1",
        RetryPolicy::try_new(3, 10, 20).expect("retry policy"),
        100,
    );
    let first_enqueue = framework.enqueue_outbound(&request).expect("enqueue");
    assert!(!first_enqueue.idempotent_replay());
    assert!(
        framework
            .enqueue_outbound(&request)
            .expect("enqueue replay")
            .idempotent_replay()
    );
    let mut connector = ConnectorFixture {
        outcomes: VecDeque::from([
            Err(connector_error(
                ConnectorCallErrorKind::Retryable,
                "REMOTE_BUSY",
            )),
            Ok(OutboundCallReceipt::try_new(digest('a'), true).expect("remote receipt")),
        ]),
        ..ConnectorFixture::default()
    };
    let first = framework
        .deliver_next(
            &tenant,
            &integration_id,
            100,
            lease('A'),
            105,
            &mut connector,
        )
        .expect("first attempt")
        .expect("due operation");
    let OutboundAttemptResult::RetryScheduled(retry) = first else {
        panic!("expected retry");
    };
    assert_eq!(retry.attempt(), 1);
    assert_eq!(retry.eligible_at_millis(), 110);
    assert!(
        framework
            .deliver_next(
                &tenant,
                &integration_id,
                109,
                lease('B'),
                115,
                &mut connector,
            )
            .expect("not due")
            .is_none()
    );
    let second = framework
        .deliver_next(
            &tenant,
            &integration_id,
            110,
            lease('C'),
            120,
            &mut connector,
        )
        .expect("second attempt")
        .expect("due retry");
    let OutboundAttemptResult::Delivered(delivered) = second else {
        panic!("expected delivery");
    };
    assert_eq!(
        delivered.operation().state(),
        OutboundOperationState::Delivered
    );
    assert_eq!(delivered.operation().attempt(), 2);
    assert_eq!(delivered.remote_receipt_digest(), Some(&digest('a')));
    assert_eq!(connector.delivery_calls.len(), 2);
    assert_eq!(connector.delivery_calls[0].0, connector.delivery_calls[1].0);
    assert_eq!(connector.delivery_calls[0].2, connector.delivery_calls[1].2);
    assert_eq!(connector.delivery_calls[0].1, 1);
    assert_eq!(connector.delivery_calls[1].1, 2);
    assert_outbound_retry_audit(&framework, &tenant, &integration_id);
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn lease_recovery_reuses_operation_and_exact_terminal_receipt() {
    let root = temporary_directory("lease-recovery");
    let integration_id = integration_id('D');
    let tenant = scope('1', '4');
    let request = outbound(
        integration_id.clone(),
        tenant.clone(),
        "outbound-operation-2",
        RetryPolicy::try_new(3, 10, 40).expect("retry policy"),
        100,
    );
    let mut storage = IntegrationStorage::open(&root).expect("store");
    storage
        .register(&registration(integration_id.clone(), tenant.clone(), '5'))
        .expect("register");
    storage.enqueue_outbound(&request).expect("enqueue");
    let abandoned = storage
        .claim_due(&tenant, &integration_id, 100, lease('D'), 105)
        .expect("claim")
        .expect("due");
    assert_eq!(abandoned.attempt(), 1);
    drop(storage);

    let mut restarted = IntegrationStorage::open(&root).expect("restart store");
    assert!(
        restarted
            .claim_due(&tenant, &integration_id, 104, lease('E'), 110)
            .expect("live lease")
            .is_none()
    );
    let recovered = restarted
        .claim_due(&tenant, &integration_id, 105, lease('F'), 115)
        .expect("expired lease")
        .expect("recovered claim");
    assert_eq!(recovered.attempt(), 2);
    assert_eq!(recovered.operation_key(), abandoned.operation_key());
    assert_eq!(recovered.request_digest(), abandoned.request_digest());
    assert_eq!(recovered.payload(), abandoned.payload());
    let remote = OutboundCallReceipt::try_new(digest('b'), false).expect("remote receipt");
    let applied = restarted
        .record_success(&tenant, &recovered, &remote, 106)
        .expect("success");
    assert!(!applied.idempotent_replay());
    let replay = restarted
        .record_success(&tenant, &recovered, &remote, 999)
        .expect("success replay");
    assert!(replay.idempotent_replay());
    assert_eq!(replay.completed_at_millis(), 106);
    assert_eq!(
        restarted
            .record_success(
                &tenant,
                &recovered,
                &OutboundCallReceipt::try_new(digest('b'), true).expect("changed receipt"),
                107,
            )
            .expect_err("changed terminal receipt")
            .kind(),
        IntegrationErrorKind::Conflict
    );
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn failure_receipt_replay_requires_the_same_failure_category() {
    let root = temporary_directory("failure-replay");
    let integration_id = integration_id('K');
    let tenant = scope('1', '4');
    let request = outbound(
        integration_id.clone(),
        tenant.clone(),
        "outbound-operation-failure-replay",
        RetryPolicy::try_new(3, 5, 20).expect("retry policy"),
        100,
    );
    let mut storage = IntegrationStorage::open(&root).expect("store");
    storage
        .register(&registration(integration_id.clone(), tenant.clone(), '5'))
        .expect("register");
    storage.enqueue_outbound(&request).expect("enqueue");
    let claim = storage
        .claim_due(&tenant, &integration_id, 100, lease('K'), 105)
        .expect("claim")
        .expect("due claim");
    let retryable = connector_error(ConnectorCallErrorKind::Retryable, "REMOTE_BUSY");
    assert!(matches!(
        storage
            .record_failure(&tenant, &claim, &retryable, 100)
            .expect("record retry"),
        OutboundAttemptResult::RetryScheduled(_)
    ));
    assert!(matches!(
        storage
            .record_failure(&tenant, &claim, &retryable, 999)
            .expect("exact retry replay"),
        OutboundAttemptResult::RetryScheduled(_)
    ));
    assert_eq!(
        storage
            .record_failure(
                &tenant,
                &claim,
                &connector_error(ConnectorCallErrorKind::Permanent, "REMOTE_BUSY"),
                999,
            )
            .expect_err("changed failure category")
            .kind(),
        IntegrationErrorKind::Conflict
    );
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn exhausted_and_permanent_failures_dead_letter_without_business_mutation() {
    let root = temporary_directory("dead-letter");
    let integration_id = integration_id('E');
    let tenant = scope('1', '4');
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    framework
        .register_connector(&registration(integration_id.clone(), tenant.clone(), '5'))
        .expect("register");
    let request = outbound(
        integration_id.clone(),
        tenant.clone(),
        "outbound-operation-3",
        RetryPolicy::try_new(2, 5, 5).expect("retry policy"),
        100,
    );
    framework.enqueue_outbound(&request).expect("enqueue");
    let mut connector = ConnectorFixture {
        outcomes: VecDeque::from([
            Err(connector_error(
                ConnectorCallErrorKind::Retryable,
                "REMOTE_BUSY",
            )),
            Err(connector_error(
                ConnectorCallErrorKind::Retryable,
                "REMOTE_BUSY",
            )),
        ]),
        ..ConnectorFixture::default()
    };
    framework
        .deliver_next(
            &tenant,
            &integration_id,
            100,
            lease('G'),
            104,
            &mut connector,
        )
        .expect("first attempt")
        .expect("result");
    let terminal = framework
        .deliver_next(
            &tenant,
            &integration_id,
            105,
            lease('H'),
            110,
            &mut connector,
        )
        .expect("second attempt")
        .expect("result");
    let OutboundAttemptResult::DeadLettered(receipt) = terminal else {
        panic!("expected dead letter");
    };
    assert_eq!(receipt.operation().attempt(), 2);
    assert_eq!(
        receipt.operation().state(),
        OutboundOperationState::DeadLetter
    );
    let permanent_request =
        deliver_permanent_failure(&mut framework, &mut connector, &tenant, &integration_id);
    assert_eq!(
        framework
            .storage()
            .inbound_dispatches(&tenant, &integration_id, 0, 10)
            .expect("business dispatches")
            .len(),
        0
    );
    drop(framework);
    let restarted = IntegrationStorage::open(&root).expect("restart store");
    assert_eq!(
        restarted
            .outbound_operation(&tenant, &integration_id, request.operation_key())
            .expect("operation")
            .state(),
        OutboundOperationState::DeadLetter
    );
    assert_eq!(
        restarted
            .outbound_operation(&tenant, &integration_id, permanent_request.operation_key())
            .expect("permanent operation")
            .state(),
        OutboundOperationState::DeadLetter
    );
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn credential_revocation_stops_inbound_and_outbound_before_connector_calls() {
    let root = temporary_directory("revocation");
    let integration_id = integration_id('F');
    let tenant = scope('1', '4');
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    let authority = framework
        .register_connector(&registration(integration_id.clone(), tenant.clone(), '5'))
        .expect("register")
        .authority()
        .clone();
    let request = outbound(
        integration_id.clone(),
        tenant.clone(),
        "outbound-operation-4",
        RetryPolicy::try_new(2, 5, 5).expect("retry policy"),
        100,
    );
    framework.enqueue_outbound(&request).expect("enqueue");
    let revoked = framework
        .revoke_credential(&tenant, &integration_id, authority.revision(), 101)
        .expect("revoke");
    assert_eq!(revoked.state(), ConnectorState::CredentialRevoked);
    assert_eq!(revoked.updated_at_millis(), 101);
    assert_eq!(
        framework
            .revoke_credential(&tenant, &integration_id, authority.revision(), 101)
            .expect("exact revoke replay"),
        revoked
    );
    assert_eq!(
        framework
            .revoke_credential(&tenant, &integration_id, authority.revision(), 102)
            .expect_err("changed revoke replay")
            .kind(),
        IntegrationErrorKind::Conflict
    );
    let mut verifier = VerifierFixture::default();
    let mut connector = ConnectorFixture::default();
    assert_eq!(
        framework
            .receive_webhook(
                &inbound(
                    integration_id.clone(),
                    tenant.clone(),
                    "post-revocation-event",
                    1,
                    b"valid-signature",
                    br#"{"value":1}"#,
                    102,
                ),
                &mut verifier,
                &mut connector,
            )
            .expect_err("revoked inbound")
            .kind(),
        IntegrationErrorKind::CredentialRevoked
    );
    assert_eq!(verifier.calls, 0);
    assert_eq!(connector.normalize_calls, 0);
    assert_eq!(
        framework
            .deliver_next(
                &tenant,
                &integration_id,
                102,
                lease('J'),
                110,
                &mut connector,
            )
            .expect_err("revoked outbound")
            .kind(),
        IntegrationErrorKind::CredentialRevoked
    );
    assert!(connector.delivery_calls.is_empty());
    assert_eq!(
        framework
            .enqueue_outbound(&outbound(
                integration_id.clone(),
                tenant.clone(),
                "outbound-operation-5",
                RetryPolicy::try_new(2, 5, 5).expect("retry policy"),
                103,
            ))
            .expect_err("revoked enqueue")
            .kind(),
        IntegrationErrorKind::CredentialRevoked
    );
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn outbound_adapter_credential_revocation_atomically_dead_letters_and_revokes() {
    let root = temporary_directory("outbound-revocation");
    let integration_id = integration_id('P');
    let tenant = scope('1', '4');
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    framework
        .register_connector(&registration(integration_id.clone(), tenant.clone(), '5'))
        .expect("register");
    let request = outbound(
        integration_id.clone(),
        tenant.clone(),
        "outbound-credential-revoked",
        RetryPolicy::try_new(3, 5, 20).expect("retry policy"),
        100,
    );
    framework.enqueue_outbound(&request).expect("enqueue");
    let mut connector = ConnectorFixture {
        outcomes: VecDeque::from([Err(connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "CREDENTIAL_REVOKED",
        ))]),
        ..ConnectorFixture::default()
    };
    let result = framework
        .deliver_next(
            &tenant,
            &integration_id,
            100,
            lease('P'),
            105,
            &mut connector,
        )
        .expect("deliver")
        .expect("result");
    assert!(matches!(result, OutboundAttemptResult::DeadLettered(_)));
    assert_eq!(connector.delivery_calls.len(), 1);
    assert_eq!(
        framework
            .storage()
            .authority(&tenant, &integration_id)
            .expect("authority")
            .state(),
        ConnectorState::CredentialRevoked
    );
    assert_eq!(
        framework
            .deliver_next(
                &tenant,
                &integration_id,
                101,
                lease('Q'),
                106,
                &mut connector
            )
            .expect_err("revoked follow-up")
            .kind(),
        IntegrationErrorKind::CredentialRevoked
    );
    assert_eq!(connector.delivery_calls.len(), 1);
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}
