// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, CredentialReferenceRevokeCommand,
    CredentialReferenceRevokeCommandCommand, CredentialReferenceRevokePayload,
    CredentialReferenceRotateCommand, CredentialReferenceRotateCommandCommand,
    CredentialReferenceRotatePayload, ModelRoute, OrganizationScope, OrganizationScopeKind, Scope,
};
use winwincode_control_plane::{
    CredentialReferenceResolution, CredentialReferenceService, FrozenModelRouteAuthority,
    ModelCapability, ModelReservationReceipt, ModelReservationReleaseReason,
    ModelReservationTerminalOutcome, ModelReservationTerminalReceipt, ModelSettingsRequest,
    ModelSettingsService, ModelSettingsTarget, ModelSettingsValues, ModelToolSupport,
    ProviderAdapterError, ProviderAdapterInvocation, ProviderAdapterOpenReceipt,
    ProviderAdapterPort, ProviderAdmissionError, ProviderAdmissionOpenReceipt,
    ProviderAdmissionOpenRequest, ProviderCatalogErrorKind, ProviderCatalogRequest,
    ProviderCatalogService, ProviderDescriptor, ProviderFinishReason, ProviderGateway,
    ProviderGatewayAdmissionPort, ProviderGatewayErrorKind, ProviderGatewayIdentity,
    ProviderGatewayIdentityError, ProviderGatewayIdentityPort, ProviderGatewayOpenReceipt,
    ProviderGatewaySettlement, ProviderGatewaySettlementError, ProviderGatewaySettlementPort,
    ProviderStreamControlAction, ProviderStreamConverter, ProviderStreamEvent,
    ProviderStreamFailure, ProviderStreamFailureKind, ProviderTokenUsage, ResolvedSecret,
    SecretStoreError, SecretStorePort, StructuredOutputSupport,
};
use winwincode_domain::{
    CodexThreadId, CredentialReferenceId, ExecutionJobId, ExecutionMessageId, FencingToken,
    Instant, LeaseId, ModelExchangeId, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, Revision, SchemaVersion, SessionIdentity, Sha256Digest, UserId, WorkerId,
    WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_execution_port::generated::{
    EncodedPayload, ExecutionLeaseStamp, ModelGatewayRoute, ModelOpenMessage, ModelOpenMessageKind,
};
use winwincode_storage::SqliteStorage;

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);
const DIFFERENTIAL_FIXTURE: &str =
    include_str!("../../../tests/fixtures/provider-dsh-rust-differential.v1.json");

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-provider-dsh-matrix-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn actor() -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", 1)),
        kind: UserActorKind::User,
    })
}

fn organization_scope() -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", 1)),
    }
}

fn repository_scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
    }
}

fn route() -> ModelRoute {
    ModelRoute {
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
        model_id: "model-matrix".to_owned(),
        provider_id: "provider-matrix".to_owned(),
    }
}

fn model(context_window_tokens: u64) -> ModelCapability {
    ModelCapability {
        model_id: "model-matrix".to_owned(),
        display_name: "Matrix model".to_owned(),
        context_window_tokens,
        max_output_tokens: 16_000,
        tool_support: ModelToolSupport::Parallel,
        structured_output_support: StructuredOutputSupport::Unsupported,
        reasoning_efforts: vec!["high".to_owned()],
    }
}

fn descriptor(version: u64) -> ProviderDescriptor {
    ProviderDescriptor {
        provider_id: "provider-matrix".to_owned(),
        display_name: format!("Matrix Provider v{version}"),
        adapter_kind: "fixture-adapter".to_owned(),
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
        models: vec![model(128_000 + version)],
    }
}

fn catalog_request(request_seed: u64, expected_catalog_version: u64) -> ProviderCatalogRequest {
    ProviderCatalogRequest {
        actor: actor(),
        scope: Scope::OrganizationScope(organization_scope()),
        request_id: RequestId(id("req", request_seed)),
        expected_catalog_version,
    }
}

fn create_credential() -> CredentialReferenceCreateCommand {
    CredentialReferenceCreateCommand {
        actor: actor(),
        command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
        expected_revision: Revision(0),
        payload: CredentialReferenceCreatePayload {
            credential_reference_id: CredentialReferenceId(id("crd", 1)),
            display_name: "Matrix credential".to_owned(),
            provider_id: "provider-matrix".to_owned(),
            vault_locator: "local-fixture://matrix-v1".to_owned(),
        },
        request_id: RequestId(id("req", 2)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::OrganizationScope(organization_scope()),
    }
}

fn rotate_credential() -> CredentialReferenceRotateCommand {
    CredentialReferenceRotateCommand {
        actor: actor(),
        command: CredentialReferenceRotateCommandCommand::CredentialReferenceRotate,
        expected_revision: Revision(1),
        payload: CredentialReferenceRotatePayload {
            credential_reference_id: CredentialReferenceId(id("crd", 1)),
            vault_locator: "local-fixture://matrix-v2".to_owned(),
        },
        request_id: RequestId(id("req", 4)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::OrganizationScope(organization_scope()),
    }
}

fn revoke_credential() -> CredentialReferenceRevokeCommand {
    CredentialReferenceRevokeCommand {
        actor: actor(),
        command: CredentialReferenceRevokeCommandCommand::CredentialReferenceRevoke,
        expected_revision: Revision(2),
        payload: CredentialReferenceRevokePayload {
            credential_reference_id: CredentialReferenceId(id("crd", 1)),
        },
        request_id: RequestId(id("req", 6)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::OrganizationScope(organization_scope()),
    }
}

fn configure(storage: &mut SqliteStorage) {
    ProviderCatalogService::new(storage)
        .upsert(&catalog_request(1, 0), &descriptor(1))
        .expect("create Provider catalog");
    CredentialReferenceService::new(storage)
        .create(&create_credential(), 1_800_000_000_000)
        .expect("create Credential reference");
    ModelSettingsService::new(storage)
        .update(
            &ModelSettingsRequest {
                actor: actor(),
                target: ModelSettingsTarget::ProductSession {
                    repository_scope: repository_scope(),
                    product_session_id: ProductSessionId(id("psn", 1)),
                },
                request_id: RequestId(id("req", 3)),
                expected_revision: 0,
            },
            ModelSettingsValues {
                default_model_route: Some(route()),
                worker_concurrency_limit: 1,
            },
        )
        .expect("configure model route");
}

fn encoded_payload(bytes: &[u8]) -> EncodedPayload {
    EncodedPayload {
        content_type: "application/json".to_owned(),
        data_base64: STANDARD.encode(bytes),
        payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
    }
}

fn open_message(seed: u64) -> ModelOpenMessage {
    let worker_session_id = WorkerSessionId(id("wsn", 1));
    ModelOpenMessage {
        kind: ModelOpenMessageKind::ModelOpen,
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: Instant("2030-01-01T00:05:00Z".to_owned()),
            fencing_token: FencingToken("1".to_owned()),
            issued_at: Instant("2030-01-01T00:00:00Z".to_owned()),
            job_id: ExecutionJobId(id("job", seed)),
            lease_id: LeaseId(id("lse", seed)),
            worker_id: WorkerId(id("wrk", 1)),
            worker_instance_id: WorkerInstanceId(id("wki", 1)),
        },
        message_id: ExecutionMessageId(id("xmsg", seed)),
        model_exchange_id: ModelExchangeId(id("mdl", seed)),
        request: encoded_payload(br#"{"prompt":"safe"}"#),
        request_id: RequestId(id("req", 100 + seed)),
        route: ModelGatewayRoute {
            capability: "reasoning".to_owned(),
            route: "matrix-route".to_owned(),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2030-01-01T00:00:01Z".to_owned()),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId(id("cdx", 1)),
            product_session_id: ProductSessionId(id("psn", 1)),
            stage_run_id: None,
            worker_session_id: worker_session_id.clone(),
        },
        worker_session_id,
    }
}

struct Identity;

impl ProviderGatewayIdentityPort for Identity {
    fn authorize(
        &self,
        message: &ModelOpenMessage,
    ) -> Result<ProviderGatewayIdentity, ProviderGatewayIdentityError> {
        Ok(ProviderGatewayIdentity::product_session(
            repository_scope(),
            message.session_identity.product_session_id.clone(),
        ))
    }
}

#[derive(Default)]
struct VersionedSecretStore {
    resolutions: Mutex<Vec<u64>>,
}

impl VersionedSecretStore {
    fn resolutions(&self) -> Vec<u64> {
        self.resolutions
            .lock()
            .expect("lock secret resolutions")
            .clone()
    }
}

impl SecretStorePort for VersionedSecretStore {
    fn resolve(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError> {
        let version = reference.rotation_version();
        self.resolutions
            .lock()
            .expect("lock secret resolutions")
            .push(version);
        ResolvedSecret::from_bytes(format!("matrix-secret-v{version}").into_bytes())
    }
}

#[derive(Clone, Default)]
struct AdapterProbe(Arc<Mutex<Vec<String>>>);

impl AdapterProbe {
    fn received_secrets(&self) -> Vec<String> {
        self.0.lock().expect("lock adapter calls").clone()
    }
}

impl ProviderAdapterPort for AdapterProbe {
    fn provider_id(&self) -> &'static str {
        "provider-matrix"
    }

    fn open(
        &self,
        invocation: &ProviderAdapterInvocation<'_>,
        credential: &ResolvedSecret,
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError> {
        self.0.lock().expect("lock adapter calls").push(
            String::from_utf8(credential.expose().to_vec()).expect("fixture secret is UTF-8"),
        );
        ProviderAdapterOpenReceipt::try_new(invocation.adapter_request_id().to_owned())
    }

    fn control(
        &self,
        _model_exchange_id: &ModelExchangeId,
        _adapter_request_id: &str,
        _action: ProviderStreamControlAction,
    ) -> Result<(), ProviderAdapterError> {
        Ok(())
    }
}

struct Settlement;

impl ProviderGatewaySettlementPort for Settlement {
    fn settle(
        &self,
        _settlement: &ProviderGatewaySettlement,
    ) -> Result<(), ProviderGatewaySettlementError> {
        Ok(())
    }
}

#[derive(Default)]
struct Admission {
    revision: u64,
}

impl ProviderGatewayAdmissionPort for Admission {
    fn reserve(
        &mut self,
        request: &ProviderAdmissionOpenRequest<'_>,
    ) -> Result<ProviderAdmissionOpenReceipt, ProviderAdmissionError> {
        let authority = FrozenModelRouteAuthority::from_resolved_authority(
            request.identity,
            request.settings,
            request.capability,
            request.credential,
        )
        .expect("freeze route authority");
        self.revision += 1;
        Ok(ProviderAdmissionOpenReceipt {
            reservation: ModelReservationReceipt {
                request_id: request.message.request_id.clone(),
                model_exchange_id: request.message.model_exchange_id.clone(),
                route_authority_fingerprint: authority.fingerprint().to_owned(),
                denial: None,
                unix_minute: 1,
                revision: self.revision,
                idempotent_replay: false,
            },
            route_authority: authority,
            enterprise_quota_amounts: winwincode_storage::EnterpriseQuotaAmounts {
                tokens: 100,
                provider_cost_micros: 10,
                operations: 1,
                ..winwincode_storage::EnterpriseQuotaAmounts::default()
            },
        })
    }

    fn release(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        reason: ModelReservationReleaseReason,
    ) -> Result<ModelReservationTerminalReceipt, ProviderAdmissionError> {
        self.revision += 1;
        Ok(ModelReservationTerminalReceipt {
            request_id: original_request_id.clone(),
            model_exchange_id: model_exchange_id.clone(),
            route_authority_fingerprint: authority.fingerprint().to_owned(),
            outcome: match reason {
                ModelReservationReleaseReason::Cancelled => {
                    ModelReservationTerminalOutcome::Cancelled
                }
                ModelReservationReleaseReason::ProviderFailed => {
                    ModelReservationTerminalOutcome::ProviderFailed
                }
            },
            actual_tokens: 0,
            actual_cost_micros: 0,
            revision: self.revision,
            idempotent_replay: false,
        })
    }

    fn complete(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        usage: ProviderTokenUsage,
        actual_cost_micros: u64,
    ) -> Result<ModelReservationTerminalReceipt, ProviderAdmissionError> {
        self.revision += 1;
        Ok(ModelReservationTerminalReceipt {
            request_id: original_request_id.clone(),
            model_exchange_id: model_exchange_id.clone(),
            route_authority_fingerprint: authority.fingerprint().to_owned(),
            outcome: ModelReservationTerminalOutcome::Completed,
            actual_tokens: usage.input_tokens + usage.output_tokens,
            actual_cost_micros,
            revision: self.revision,
            idempotent_replay: false,
        })
    }
}

fn register_adapter(gateway: &mut ProviderGateway<'_>, adapter: &AdapterProbe) {
    gateway
        .register_adapter(Box::new(adapter.clone()))
        .expect("register matrix adapter");
}

fn open_receipt(root: &Path) -> ProviderGatewayOpenReceipt {
    let mut storage = SqliteStorage::open(root).expect("open matrix storage");
    configure(&mut storage);
    let secret_store = VersionedSecretStore::default();
    let identity = Identity;
    let settlement = Settlement;
    let mut admission = Admission::default();
    let adapter = AdapterProbe::default();
    let mut gateway = ProviderGateway::new(
        &mut storage,
        &secret_store,
        &identity,
        &settlement,
        &mut admission,
    );
    register_adapter(&mut gateway, &adapter);
    gateway
        .open(&open_message(1), &route(), "adapter-matrix-1")
        .expect("open matrix exchange")
}

fn terminal_value(receipt: &ProviderGatewayOpenReceipt, event: ProviderStreamEvent) -> Value {
    let mut converter = ProviderStreamConverter::from_gateway_receipt(receipt);
    converter
        .ingest(ProviderStreamEvent::ResponseStarted {
            provider_response_id: "response-matrix".to_owned(),
        })
        .expect("start matrix response");
    let frames = converter.ingest(event).expect("convert terminal event");
    serde_json::from_str(frames.last().expect("terminal frame").payload_json())
        .expect("terminal JSON")
}

fn scenario(id: &str) -> Value {
    serde_json::from_str::<Value>(DIFFERENTIAL_FIXTURE)
        .expect("parse canonical Provider differential fixture")["scenarios"]
        .as_array()
        .expect("fixture scenarios")
        .iter()
        .find(|scenario| scenario["id"] == id)
        .unwrap_or_else(|| panic!("missing canonical scenario {id}"))
        .clone()
}

fn apply_hot_update_and_reject_invalid(storage: &mut SqliteStorage) {
    CredentialReferenceService::new(storage)
        .rotate(&rotate_credential(), 1_800_000_001_000)
        .expect("rotate Credential reference");
    ProviderCatalogService::new(storage)
        .upsert(&catalog_request(5, 1), &descriptor(2))
        .expect("hot-update Provider catalog");
    let last_good = ProviderCatalogService::new(storage)
        .project(&Scope::OrganizationScope(organization_scope()))
        .expect("project accepted catalog");
    let last_good_json = serde_json::to_vec(&last_good).expect("encode accepted catalog");
    let mut invalid = descriptor(3);
    invalid.models.push(model(999_999));
    let rejected = ProviderCatalogService::new(storage)
        .upsert(&catalog_request(50, 2), &invalid)
        .expect_err("duplicate model update is invalid");
    assert_eq!(rejected.kind(), ProviderCatalogErrorKind::InvalidRequest);
    assert_eq!(
        serde_json::to_vec(
            &ProviderCatalogService::new(storage)
                .project(&Scope::OrganizationScope(organization_scope()))
                .expect("project last-good catalog")
        )
        .expect("encode last-good catalog"),
        last_good_json
    );
}

fn stream_failure_mapping(code: &str) -> Option<ProviderStreamFailureKind> {
    match code {
        "AUTH" => Some(ProviderStreamFailureKind::Authentication),
        "QUOTA" => Some(ProviderStreamFailureKind::Quota),
        "RATE_LIMIT" => Some(ProviderStreamFailureKind::RateLimit),
        "INVALID_REQUEST" => Some(ProviderStreamFailureKind::InvalidRequest),
        "SERVER" => Some(ProviderStreamFailureKind::Server),
        "TIMEOUT" => Some(ProviderStreamFailureKind::Timeout),
        "TRANSPORT" => Some(ProviderStreamFailureKind::Transport),
        "CONTEXT_WINDOW_EXCEEDED" => Some(ProviderStreamFailureKind::ContextWindowExceeded),
        "PI_AI_ERROR" => Some(ProviderStreamFailureKind::Unknown),
        "EMPTY_RESPONSE" => None,
        _ => panic!("unsupported canonical fixture failure {code}"),
    }
}

fn assert_hot_update_contract_fixture() {
    let hot_update = scenario("hot-update");
    assert_eq!(
        hot_update["input"]["initialModelIds"],
        serde_json::json!(["model-alpha", "model-beta"])
    );
    assert!(
        hot_update["expectedFacts"]
            .as_array()
            .expect("hot-update facts")
            .iter()
            .any(|fact| fact == "the next request observes the updated provider configuration")
    );
    let credential_failure = scenario("credential-failure");
    assert_eq!(
        credential_failure["input"]["secretMarker"],
        "TOKEN-provider-differential-secret"
    );
    assert!(
        credential_failure["expectedFacts"]
            .as_array()
            .expect("credential failure facts")
            .iter()
            .any(|fact| fact == "revoked credential references fail before secret-store access")
    );
}

fn assert_revoke_blocks_new_request(
    storage: &mut SqliteStorage,
    secret_store: &VersionedSecretStore,
    admission: &mut Admission,
    adapter: &AdapterProbe,
) {
    CredentialReferenceService::new(storage)
        .revoke(&revoke_credential(), 1_800_000_002_000)
        .expect("revoke Credential reference");
    let identity = Identity;
    let settlement = Settlement;
    let mut gateway =
        ProviderGateway::new(storage, secret_store, &identity, &settlement, admission);
    register_adapter(&mut gateway, adapter);
    let revoked = gateway
        .open(&open_message(3), &route(), "adapter-matrix-3")
        .expect_err("revoked Credential blocks a new request");
    assert_eq!(
        revoked.kind(),
        ProviderGatewayErrorKind::CredentialUnavailable
    );
    assert_eq!(secret_store.resolutions(), [1, 2]);
    assert_eq!(adapter.received_secrets().len(), 2);
}

#[test]
fn provider_failure_matrix_preserves_all_ten_canonical_error_codes() {
    let typed_errors = scenario("typed-errors");
    assert_eq!(
        typed_errors["input"]["failures"]
            .as_array()
            .expect("typed error inputs")
            .iter()
            .map(|failure| failure["code"].as_str().expect("failure code"))
            .collect::<Vec<_>>(),
        [
            "AUTH",
            "QUOTA",
            "RATE_LIMIT",
            "INVALID_REQUEST",
            "SERVER",
            "TIMEOUT",
            "TRANSPORT",
            "CONTEXT_WINDOW_EXCEEDED",
            "PI_AI_ERROR",
            "EMPTY_RESPONSE",
        ]
    );
    assert!(
        typed_errors["expectedFacts"]
            .as_array()
            .expect("typed error facts")
            .iter()
            .any(|fact| {
                fact == "all ten DSH error categories remain distinct or have one approved canonical name"
            })
    );
    let root = temporary_directory("failure-matrix");
    let receipt = open_receipt(&root);
    for failure in typed_errors["input"]["failures"]
        .as_array()
        .expect("typed error inputs")
    {
        let input_code = failure["code"].as_str().expect("fixture failure code");
        let Some(kind) = stream_failure_mapping(input_code) else {
            continue;
        };
        let expected_code = failure["canonicalCode"].as_str().unwrap_or(input_code);
        let mut provider_failure = ProviderStreamFailure::new(kind);
        if let Some(status) = failure["status"].as_u64() {
            provider_failure = provider_failure
                .with_status(u16::try_from(status).expect("fixture status fits Provider status"));
        }
        if let Some(retry_after) = failure["retryAfterMillis"].as_u64() {
            provider_failure = provider_failure.with_retry_after_millis(retry_after);
        }
        let terminal = terminal_value(&receipt, ProviderStreamEvent::Failed(provider_failure));
        assert_eq!(terminal["type"], "error");
        assert_eq!(terminal["error"]["code"], expected_code);
        assert_eq!(
            terminal["error"].get("status").and_then(Value::as_u64),
            failure["status"].as_u64()
        );
        assert_eq!(
            terminal["error"]
                .get("providerRetryAfterMillis")
                .and_then(Value::as_u64),
            failure["retryAfterMillis"].as_u64()
        );
        assert!(terminal["error"].get("providerRequestId").is_none());
    }

    let raw_provider_request_id = "provider-request-redacted";
    let hashed = terminal_value(
        &receipt,
        ProviderStreamEvent::Failed(
            ProviderStreamFailure::new(ProviderStreamFailureKind::Unknown)
                .with_provider_request_id(raw_provider_request_id.to_owned()),
        ),
    );
    assert_eq!(
        hashed["error"]["providerRequestId"],
        format!(
            "sha256:{:x}",
            Sha256::digest(raw_provider_request_id.as_bytes())
        )
    );
    assert!(!hashed.to_string().contains(raw_provider_request_id));

    let empty = terminal_value(
        &receipt,
        ProviderStreamEvent::Finished(ProviderFinishReason::Stop),
    );
    assert_eq!(empty["type"], "error");
    assert_eq!(empty["error"]["code"], "EMPTY_RESPONSE");
    fs::remove_dir_all(root).expect("remove failure matrix fixture");
}

#[test]
fn open_snapshot_survives_catalog_and_credential_rotation_while_next_open_uses_new_authority() {
    assert_hot_update_contract_fixture();
    let root = temporary_directory("hot-update");
    let mut storage = SqliteStorage::open(&root).expect("open hot-update storage");
    configure(&mut storage);
    let secret_store = VersionedSecretStore::default();
    let identity = Identity;
    let settlement = Settlement;
    let mut admission = Admission::default();
    let adapter = AdapterProbe::default();

    let (first, frozen, first_authority_json) = {
        let mut gateway = ProviderGateway::new(
            &mut storage,
            &secret_store,
            &identity,
            &settlement,
            &mut admission,
        );
        register_adapter(&mut gateway, &adapter);
        let receipt = gateway
            .open(&open_message(1), &route(), "adapter-matrix-1")
            .expect("open version-one request");
        let frozen = gateway
            .durable_exchange(&receipt.model_exchange_id)
            .expect("snapshot version-one exchange");
        let authority_json = frozen
            .route_authority()
            .to_durable_json()
            .expect("encode version-one authority");
        (receipt, frozen, authority_json)
    };

    apply_hot_update_and_reject_invalid(&mut storage);

    let second = {
        let mut gateway = ProviderGateway::new(
            &mut storage,
            &secret_store,
            &identity,
            &settlement,
            &mut admission,
        );
        register_adapter(&mut gateway, &adapter);
        let replay = gateway
            .restore_durable_exchange(&frozen)
            .expect("restore exact frozen request without current authority lookup");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.route, first.route);
        assert_eq!(
            frozen
                .route_authority()
                .to_durable_json()
                .expect("re-encode frozen authority"),
            first_authority_json
        );
        let receipt = gateway
            .open(&open_message(2), &route(), "adapter-matrix-2")
            .expect("open version-two request");
        let current = gateway
            .durable_exchange(&receipt.model_exchange_id)
            .expect("snapshot version-two exchange");
        assert_ne!(
            current.route_authority().fingerprint(),
            frozen.route_authority().fingerprint()
        );
        receipt
    };
    assert_eq!(secret_store.resolutions(), [1, 2]);
    assert_eq!(
        adapter.received_secrets(),
        ["matrix-secret-v1", "matrix-secret-v2"]
    );
    assert_eq!(second.route, first.route);

    assert_revoke_blocks_new_request(&mut storage, &secret_store, &mut admission, &adapter);
    drop(storage);
    fs::remove_dir_all(root).expect("remove hot-update fixture");
}
