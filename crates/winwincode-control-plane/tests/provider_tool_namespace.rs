// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, ModelRoute, OrganizationScope, OrganizationScopeKind, Scope,
};
use winwincode_control_plane::{
    CredentialReferenceResolution, CredentialReferenceService, FrozenModelRouteAuthority,
    ModelCapability, ModelReservationReceipt, ModelReservationReleaseReason,
    ModelReservationTerminalOutcome, ModelReservationTerminalReceipt, ModelSettingsRequest,
    ModelSettingsService, ModelSettingsTarget, ModelSettingsValues, ModelToolSupport,
    ProviderAdapterError, ProviderAdapterInvocation, ProviderAdapterOpenReceipt,
    ProviderAdapterPort, ProviderAdmissionError, ProviderAdmissionOpenReceipt,
    ProviderAdmissionOpenRequest, ProviderCatalogRequest, ProviderCatalogService,
    ProviderDescriptor, ProviderFinishReason, ProviderGateway, ProviderGatewayAdmissionPort,
    ProviderGatewayIdentity, ProviderGatewayIdentityError, ProviderGatewayIdentityPort,
    ProviderGatewayOpenReceipt, ProviderGatewaySettlement, ProviderGatewaySettlementError,
    ProviderGatewaySettlementPort, ProviderStreamControlAction, ProviderStreamConverter,
    ProviderStreamEvent, ProviderTokenUsage, ProviderToolIdentity, ProviderToolIdentityError,
    ProviderToolKind, ResolvedSecret, SecretStoreError, SecretStorePort, StructuredOutputSupport,
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

const PROVIDER_SECRET: &[u8] = b"provider-tool-namespace-secret-fixture";
static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn temporary_directory() -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-provider-tool-namespace-{}-{suffix}",
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

struct SecretStore;

impl SecretStorePort for SecretStore {
    fn resolve(
        &self,
        _reference: &CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError> {
        ResolvedSecret::from_bytes(PROVIDER_SECRET.to_vec())
    }
}

struct Adapter;

impl ProviderAdapterPort for Adapter {
    fn provider_id(&self) -> &'static str {
        "provider-tool-namespace"
    }

    fn open(
        &self,
        invocation: &ProviderAdapterInvocation<'_>,
        credential: &ResolvedSecret,
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError> {
        assert_eq!(credential.expose(), PROVIDER_SECRET);
        assert_eq!(invocation.model_id(), "model-tool-namespace");
        ProviderAdapterOpenReceipt::try_new("adapter-tool-namespace".to_owned())
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

struct Admission;

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
        .expect("namespace fixture admission authority");
        Ok(ProviderAdmissionOpenReceipt {
            reservation: ModelReservationReceipt {
                request_id: request.message.request_id.clone(),
                model_exchange_id: request.message.model_exchange_id.clone(),
                route_authority_fingerprint: authority.fingerprint().to_owned(),
                denial: None,
                unix_minute: 1,
                revision: 1,
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
            revision: 2,
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
        Ok(ModelReservationTerminalReceipt {
            request_id: original_request_id.clone(),
            model_exchange_id: model_exchange_id.clone(),
            route_authority_fingerprint: authority.fingerprint().to_owned(),
            outcome: ModelReservationTerminalOutcome::Completed,
            actual_tokens: usage.input_tokens + usage.output_tokens,
            actual_cost_micros,
            revision: 2,
            idempotent_replay: false,
        })
    }
}

fn encoded_payload(bytes: &[u8]) -> EncodedPayload {
    EncodedPayload {
        content_type: "application/json".to_owned(),
        data_base64: STANDARD.encode(bytes),
        payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
    }
}

fn open_message() -> ModelOpenMessage {
    let worker_session_id = WorkerSessionId(id("wsn", 1));
    ModelOpenMessage {
        kind: ModelOpenMessageKind::ModelOpen,
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: Instant("2030-01-01T00:05:00Z".to_owned()),
            fencing_token: FencingToken("1".to_owned()),
            issued_at: Instant("2030-01-01T00:00:00Z".to_owned()),
            job_id: ExecutionJobId(id("job", 1)),
            lease_id: LeaseId(id("lse", 1)),
            worker_id: WorkerId(id("wrk", 1)),
            worker_instance_id: WorkerInstanceId(id("wki", 1)),
        },
        message_id: ExecutionMessageId(id("xmsg", 1)),
        model_exchange_id: ModelExchangeId(id("mdl", 1)),
        request: encoded_payload(br#"{"prompt":"safe"}"#),
        request_id: RequestId(id("req", 100)),
        route: ModelGatewayRoute {
            capability: "reasoning".to_owned(),
            route: "namespace-route".to_owned(),
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

fn route() -> ModelRoute {
    ModelRoute {
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
        provider_id: "provider-tool-namespace".to_owned(),
        model_id: "model-tool-namespace".to_owned(),
    }
}

fn configure_storage(storage: &mut SqliteStorage) {
    ProviderCatalogService::new(storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: actor(),
                scope: Scope::OrganizationScope(organization_scope()),
                request_id: RequestId(id("req", 1)),
                expected_catalog_version: 0,
            },
            &ProviderDescriptor {
                provider_id: "provider-tool-namespace".to_owned(),
                display_name: "Provider Tool Namespace".to_owned(),
                adapter_kind: "fixture-adapter".to_owned(),
                credential_reference_id: CredentialReferenceId(id("crd", 1)),
                models: vec![ModelCapability {
                    model_id: "model-tool-namespace".to_owned(),
                    display_name: "Model Tool Namespace".to_owned(),
                    context_window_tokens: 128_000,
                    max_output_tokens: 16_000,
                    tool_support: ModelToolSupport::Parallel,
                    structured_output_support: StructuredOutputSupport::Unsupported,
                    reasoning_efforts: vec!["high".to_owned()],
                }],
            },
        )
        .expect("register namespace fixture Provider");
    CredentialReferenceService::new(storage)
        .create(
            &CredentialReferenceCreateCommand {
                actor: actor(),
                command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
                expected_revision: Revision(0),
                payload: CredentialReferenceCreatePayload {
                    credential_reference_id: CredentialReferenceId(id("crd", 1)),
                    display_name: "Namespace fixture credential".to_owned(),
                    provider_id: "provider-tool-namespace".to_owned(),
                    vault_locator: "local-fixture://provider-tool-namespace".to_owned(),
                },
                request_id: RequestId(id("req", 2)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::OrganizationScope(organization_scope()),
            },
            1_800_000_000_000,
        )
        .expect("create namespace fixture Credential reference");
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
        .expect("configure namespace fixture route");
}

fn gateway_receipt() -> ProviderGatewayOpenReceipt {
    let root = temporary_directory();
    let mut storage = SqliteStorage::open(&root).expect("open namespace fixture storage");
    configure_storage(&mut storage);
    let secret_store = SecretStore;
    let identity = Identity;
    let settlement = Settlement;
    let mut admission = Admission;
    let receipt = {
        let mut gateway = ProviderGateway::new(
            &mut storage,
            &secret_store,
            &identity,
            &settlement,
            &mut admission,
        );
        gateway
            .register_adapter(Box::new(Adapter))
            .expect("register namespace fixture adapter");
        gateway
            .open(&open_message(), &route(), "adapter-tool-namespace")
            .expect("open namespace fixture exchange")
    };
    drop(storage);
    fs::remove_dir_all(root).expect("remove namespace fixture storage");
    receipt
}

fn identity(kind: ProviderToolKind, name: &str, namespace: Option<&str>) -> ProviderToolIdentity {
    ProviderToolIdentity::try_new(kind, name.to_owned(), namespace.map(str::to_owned))
        .expect("valid Provider tool identity")
}

#[test]
fn canonical_tool_identity_has_stable_json_and_exact_boundaries() {
    let namespaced = identity(ProviderToolKind::Function, "read_file", Some("workspace_1"));
    assert_eq!(namespaced.kind(), ProviderToolKind::Function);
    assert_eq!(namespaced.name(), "read_file");
    assert_eq!(namespaced.namespace(), Some("workspace_1"));
    assert_eq!(
        serde_json::to_string(&namespaced).expect("serialize stable namespaced identity"),
        r#"{"kind":"function","name":"read_file","namespace":"workspace_1"}"#
    );
    assert_eq!(
        serde_json::to_value(&namespaced).expect("serialize namespaced identity"),
        json!({
            "kind": "function",
            "name": "read_file",
            "namespace": "workspace_1"
        })
    );

    let root = identity(ProviderToolKind::Custom, "apply_patch", None);
    assert_eq!(
        serde_json::to_string(&root).expect("serialize stable root identity"),
        r#"{"kind":"custom","name":"apply_patch"}"#
    );
    assert_eq!(
        serde_json::to_value(&root).expect("serialize root identity"),
        json!({"kind": "custom", "name": "apply_patch"})
    );

    let boundary = ProviderToolIdentity::try_new(
        ProviderToolKind::Function,
        "n".repeat(128),
        Some("s".repeat(64)),
    )
    .expect("accept exact canonical identity bounds");
    assert_eq!(boundary.name().len(), 128);
    assert_eq!(boundary.namespace().map(str::len), Some(64));
}

#[test]
fn canonical_tool_identity_rejects_illegal_empty_and_overlong_components() {
    for name in [
        String::new(),
        "bad name".to_owned(),
        "bad/name".to_owned(),
        "工具".to_owned(),
        "n".repeat(129),
    ] {
        assert_eq!(
            ProviderToolIdentity::try_new(ProviderToolKind::Function, name, None),
            Err(ProviderToolIdentityError::InvalidName)
        );
    }

    for namespace in [
        String::new(),
        "bad namespace".to_owned(),
        "bad/namespace".to_owned(),
        "命名空间".to_owned(),
        "s".repeat(65),
    ] {
        assert_eq!(
            ProviderToolIdentity::try_new(
                ProviderToolKind::Custom,
                "apply_patch".to_owned(),
                Some(namespace),
            ),
            Err(ProviderToolIdentityError::InvalidNamespace)
        );
    }
}

#[test]
fn namespaced_parallel_tool_calls_round_trip_in_added_and_done_items() {
    let receipt = gateway_receipt();
    let mut converter = ProviderStreamConverter::from_gateway_receipt(&receipt);
    let events = [
        ProviderStreamEvent::ResponseStarted {
            provider_response_id: "response-tool-namespace".to_owned(),
        },
        ProviderStreamEvent::ToolCallStarted {
            index: 10,
            provider_call_id: "call-workspace".to_owned(),
            identity: identity(ProviderToolKind::Function, "search", Some("workspace")),
        },
        ProviderStreamEvent::ToolCallStarted {
            index: 11,
            provider_call_id: "call-account".to_owned(),
            identity: identity(ProviderToolKind::Custom, "search", Some("account")),
        },
        ProviderStreamEvent::ToolCallArgumentsDelta {
            index: 10,
            provider_call_id: "call-workspace".to_owned(),
            delta: r#"{"path":"README.md"}"#.to_owned(),
        },
        ProviderStreamEvent::ToolCallArgumentsDelta {
            index: 11,
            provider_call_id: "call-account".to_owned(),
            delta: "query".to_owned(),
        },
        ProviderStreamEvent::ToolCallEnded {
            index: 11,
            provider_call_id: "call-account".to_owned(),
        },
        ProviderStreamEvent::ToolCallEnded {
            index: 10,
            provider_call_id: "call-workspace".to_owned(),
        },
        ProviderStreamEvent::Finished(ProviderFinishReason::ToolCalls),
    ];
    let values = events
        .into_iter()
        .flat_map(|event| converter.ingest(event).expect("convert namespace event"))
        .map(|frame| {
            serde_json::from_str::<Value>(frame.payload_json()).expect("canonical ModelPort JSON")
        })
        .collect::<Vec<_>>();

    let added = values
        .iter()
        .filter(|value| value["type"] == "output_item_added")
        .map(|value| &value["item"])
        .collect::<Vec<_>>();
    assert_eq!(added.len(), 2);
    assert_eq!(added[0]["type"], "function_call");
    assert_eq!(added[0]["name"], "search");
    assert_eq!(added[0]["namespace"], "workspace");
    assert_eq!(added[0]["call_id"], "call-workspace");
    assert_eq!(added[1]["type"], "custom_tool_call");
    assert_eq!(added[1]["name"], "search");
    assert_eq!(added[1]["namespace"], "account");
    assert_eq!(added[1]["call_id"], "call-account");

    let done = values
        .iter()
        .filter(|value| value["type"] == "output_item_done")
        .map(|value| &value["item"])
        .collect::<Vec<_>>();
    assert_eq!(done.len(), 2);
    assert_eq!(done[0]["type"], "custom_tool_call");
    assert_eq!(done[0]["name"], "search");
    assert_eq!(done[0]["namespace"], "account");
    assert_eq!(done[0]["input"], "query");
    assert_eq!(done[1]["type"], "function_call");
    assert_eq!(done[1]["name"], "search");
    assert_eq!(done[1]["namespace"], "workspace");
    assert_eq!(done[1]["arguments"], r#"{"path":"README.md"}"#);
    assert_eq!(values.last().expect("terminal")["endTurn"], false);
}
