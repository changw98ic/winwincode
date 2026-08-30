// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
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
    CredentialReferenceCreatePayload, ModelRoute, OrganizationScope, OrganizationScopeKind,
    RepositoryScope, RepositoryScopeKind, Scope, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    CanonicalModelStreamFrame, CredentialReferenceResolution, CredentialReferenceService,
    FrozenModelRouteAuthority, ModelCapability, ModelFrameWriteStatus, ModelRequestAdmission,
    ModelRequestPool, ModelRequestPoolConfig, ModelRequestState, ModelReservationReceipt,
    ModelReservationReleaseReason, ModelReservationTerminalOutcome,
    ModelReservationTerminalReceipt, ModelSettingsRequest, ModelSettingsService,
    ModelSettingsTarget, ModelSettingsValues, ModelStreamFlowCoordinator, ModelStreamReadControl,
    ModelToolSupport, ProviderAdapterError, ProviderAdapterInvocation, ProviderAdapterOpenReceipt,
    ProviderAdapterPort, ProviderAdmissionError, ProviderAdmissionOpenReceipt,
    ProviderAdmissionOpenRequest, ProviderCatalogRequest, ProviderCatalogService,
    ProviderDescriptor, ProviderFinishReason, ProviderGateway, ProviderGatewayAdmissionPort,
    ProviderGatewayIdentity, ProviderGatewayIdentityError, ProviderGatewayIdentityPort,
    ProviderGatewayOpenReceipt, ProviderGatewaySettlement, ProviderGatewaySettlementError,
    ProviderGatewaySettlementPort, ProviderStreamControlAction, ProviderStreamConversionErrorKind,
    ProviderStreamConverter, ProviderStreamEvent, ProviderStreamFailure, ProviderStreamFailureKind,
    ProviderTokenUsage, ProviderToolIdentity, ProviderToolKind, ResolvedSecret, SecretStoreError,
    SecretStorePort,
};
use winwincode_domain::{
    CodexThreadId, CredentialReferenceId, ExecutionAckSequence, ExecutionJobId, ExecutionMessageId,
    FencingToken, Instant, LeaseId, ModelExchangeId, OrganizationId, ProductSessionId, ProjectId,
    RepositoryId, RequestId, Revision, SchemaVersion, SessionIdentity, Sha256Digest, UserId,
    WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::generated::{
    EncodedPayload, ExecutionLeaseStamp, ExecutionPortError, ExecutionPortErrorCode,
    LeaseWriteStatus, ModelAckMessage, ModelAckMessageKind, ModelGatewayRoute, ModelOpenMessage,
    ModelOpenMessageKind,
};
use winwincode_storage::SqliteStorage;

const PROVIDER_SECRET: &[u8] = b"provider-stream-secret-fixture";
static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn temporary_directory() -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-provider-stream-{}-{suffix}",
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

#[derive(Clone, Default)]
struct Adapter {
    controls: Arc<Mutex<Vec<ProviderStreamControlAction>>>,
}

impl Adapter {
    fn controls(&self) -> Vec<ProviderStreamControlAction> {
        self.controls.lock().expect("adapter controls").clone()
    }
}

impl ProviderAdapterPort for Adapter {
    fn provider_id(&self) -> &'static str {
        "provider-stream"
    }

    fn open(
        &self,
        invocation: &ProviderAdapterInvocation<'_>,
        credential: &ResolvedSecret,
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError> {
        assert_eq!(credential.expose(), PROVIDER_SECRET);
        assert_eq!(invocation.model_id(), "model-stream");
        ProviderAdapterOpenReceipt::try_new("adapter-request-stream".to_owned())
    }

    fn control(
        &self,
        _model_exchange_id: &ModelExchangeId,
        _adapter_request_id: &str,
        action: ProviderStreamControlAction,
    ) -> Result<(), ProviderAdapterError> {
        self.controls.lock().expect("adapter controls").push(action);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct Settlement(Arc<AtomicU64>);

impl Settlement {
    fn calls(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl ProviderGatewaySettlementPort for Settlement {
    fn settle(
        &self,
        _settlement: &ProviderGatewaySettlement,
    ) -> Result<(), ProviderGatewaySettlementError> {
        self.0.fetch_add(1, Ordering::Relaxed);
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
        .expect("stream admission authority");
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
            route: "stream-route".to_owned(),
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

fn cancellation_ack(message: &ModelOpenMessage) -> ModelAckMessage {
    ModelAckMessage {
        ack_sequence: ExecutionAckSequence(0),
        error: Some(ExecutionPortError {
            code: ExecutionPortErrorCode::Cancelled,
            message: "model exchange cancelled by Worker".to_owned(),
            retryable: false,
        }),
        kind: ModelAckMessageKind::ModelAck,
        lease: message.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", 90)),
        model_exchange_id: message.model_exchange_id.clone(),
        replay_from_sequence: None,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2030-01-01T00:00:02Z".to_owned()),
        session_identity: message.session_identity.clone(),
        status: LeaseWriteStatus::RejectedConflict,
        worker_session_id: message.worker_session_id.clone(),
    }
}

fn configure_stream_storage(storage: &mut SqliteStorage) {
    ProviderCatalogService::new(storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: actor(),
                scope: Scope::OrganizationScope(organization_scope()),
                request_id: RequestId(id("req", 1)),
                expected_catalog_version: 0,
            },
            &ProviderDescriptor {
                provider_id: "provider-stream".to_owned(),
                display_name: "Provider Stream".to_owned(),
                adapter_kind: "fixture-adapter".to_owned(),
                credential_reference_id: CredentialReferenceId(id("crd", 1)),
                models: vec![ModelCapability {
                    model_id: "model-stream".to_owned(),
                    display_name: "Model Stream".to_owned(),
                    context_window_tokens: 128_000,
                    max_output_tokens: 16_000,
                    tool_support: ModelToolSupport::Parallel,
                    reasoning_efforts: vec!["high".to_owned()],
                }],
            },
        )
        .expect("register stream Provider");
    CredentialReferenceService::new(storage)
        .create(
            &CredentialReferenceCreateCommand {
                actor: actor(),
                command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
                expected_revision: Revision(0),
                payload: CredentialReferenceCreatePayload {
                    credential_reference_id: CredentialReferenceId(id("crd", 1)),
                    display_name: "Stream credential".to_owned(),
                    provider_id: "provider-stream".to_owned(),
                    vault_locator: "local-fixture://provider-stream".to_owned(),
                },
                request_id: RequestId(id("req", 2)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::OrganizationScope(organization_scope()),
            },
            1_800_000_000_000,
        )
        .expect("create stream Credential reference");
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
                default_model_route: Some(ModelRoute {
                    credential_reference_id: CredentialReferenceId(id("crd", 1)),
                    provider_id: "provider-stream".to_owned(),
                    model_id: "model-stream".to_owned(),
                }),
                worker_concurrency_limit: 1,
            },
        )
        .expect("configure stream route");
}

fn gateway_receipt() -> ProviderGatewayOpenReceipt {
    let settlement = Settlement::default();
    with_gateway(Adapter::default(), &settlement, |gateway| {
        gateway
            .open(
                &open_message(),
                &configured_route(),
                "adapter-request-stream",
            )
            .expect("open stream Gateway exchange")
    })
}

fn configured_route() -> ModelRoute {
    ModelRoute {
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
        model_id: "model-stream".to_owned(),
        provider_id: "provider-stream".to_owned(),
    }
}

fn with_gateway<T>(
    adapter: Adapter,
    settlement: &Settlement,
    run: impl FnOnce(&mut ProviderGateway<'_>) -> T,
) -> T {
    let root = temporary_directory();
    let mut storage = SqliteStorage::open(&root).expect("open stream fixture storage");
    configure_stream_storage(&mut storage);

    let secret_store = SecretStore;
    let identity = Identity;
    let mut admission = Admission;
    let result = {
        let mut gateway = ProviderGateway::new(
            &mut storage,
            &secret_store,
            &identity,
            settlement,
            &mut admission,
        );
        gateway
            .register_adapter(Box::new(adapter))
            .expect("register stream adapter");
        run(&mut gateway)
    };
    drop(storage);
    fs::remove_dir_all(root).expect("remove stream fixture storage");
    result
}

fn convert(
    receipt: &ProviderGatewayOpenReceipt,
    events: Vec<ProviderStreamEvent>,
) -> Vec<CanonicalModelStreamFrame> {
    let mut converter = ProviderStreamConverter::from_gateway_receipt(receipt);
    events
        .into_iter()
        .flat_map(|event| converter.ingest(event).expect("convert stream event"))
        .collect()
}

fn values(frames: &[CanonicalModelStreamFrame]) -> Vec<Value> {
    frames
        .iter()
        .map(|frame| serde_json::from_str(frame.payload_json()).expect("canonical JSON"))
        .collect()
}

#[test]
fn flow_coordinator_drives_provider_watermarks_and_cancellation_once() {
    let adapter = Adapter::default();
    let adapter_probe = adapter.clone();
    let settlement = Settlement::default();
    let settlement_probe = settlement.clone();
    with_gateway(adapter, &settlement, |gateway| {
        let message = open_message();
        let receipt = gateway
            .open(&message, &configured_route(), "adapter-request-stream")
            .expect("open coordinated stream");
        let identity = ProviderGatewayIdentity::product_session(
            repository_scope(),
            message.session_identity.product_session_id.clone(),
        );
        let active = ModelRequestAdmission::from_gateway_open(&identity, &receipt)
            .expect("active pool admission");
        let waiting = ModelRequestAdmission::from_gateway_route(
            &identity,
            &receipt.route,
            ModelExchangeId(id("mdl", 2)),
            RequestId(id("req", 101)),
        )
        .expect("waiting pool admission");
        let mut pool = ModelRequestPool::new(ModelRequestPoolConfig {
            max_routes: 2,
            max_active_per_route: 1,
            max_waiting_per_route: 1,
            max_exchange_records_per_route: 4,
            max_buffered_frames_per_stream: 2,
            max_buffered_bytes_per_stream: 4_096,
            resume_buffered_frames_per_stream: 1,
            resume_buffered_bytes_per_stream: 2_048,
        })
        .expect("coordinated request pool");
        pool.submit(&active).expect("start coordinated exchange");
        pool.submit(&waiting).expect("queue same-route exchange");

        let mut converter = ProviderStreamConverter::from_gateway_receipt(&receipt);
        let frames = converter
            .ingest(ProviderStreamEvent::ResponseStarted {
                provider_response_id: "response-flow-control".to_owned(),
            })
            .expect("convert Provider start");
        let cancellation = cancellation_ack(&message);
        {
            let mut flow = ModelStreamFlowCoordinator::new(&mut pool, gateway);
            let write = flow
                .offer_provider_batch(&message.model_exchange_id, &frames, None, &message.sent_at)
                .expect("offer hard-watermark batch");
            assert_eq!(write.pool.status, ModelFrameWriteStatus::Accepted);
            assert_eq!(write.pool.read_control, ModelStreamReadControl::Paused);
            let ack = flow
                .acknowledge(&message.model_exchange_id, 1)
                .expect("ack to low watermark");
            assert_eq!(ack.pool.read_control, ModelStreamReadControl::Read);

            let cancelled = flow
                .cancel_from_worker(&cancellation)
                .expect("cancel across Gateway and pool");
            assert!(!cancelled.gateway.idempotent_replay);
            assert_eq!(
                cancelled.pool.granted_exchange_id,
                Some(waiting.model_exchange_id.clone())
            );
            let replay = flow
                .cancel_from_worker(&cancellation)
                .expect("exact cross-authority cancellation replay");
            assert!(replay.gateway.idempotent_replay);
            assert!(replay.pool.replayed);
            assert!(replay.pool.granted_exchange_id.is_none());
        }
        assert_eq!(
            pool.reconnect(&waiting.model_exchange_id)
                .expect("waiting admission released")
                .state,
            ModelRequestState::Active
        );
    });
    assert_eq!(
        adapter_probe.controls(),
        [
            ProviderStreamControlAction::Pause,
            ProviderStreamControlAction::Resume,
            ProviderStreamControlAction::Cancel,
            ProviderStreamControlAction::Release,
        ]
    );
    assert_eq!(settlement_probe.calls(), 1);
}

fn positive_events(fragmented: bool) -> Vec<ProviderStreamEvent> {
    let mut events = vec![
        ProviderStreamEvent::ResponseStarted {
            provider_response_id: "response-stream-1".to_owned(),
        },
        ProviderStreamEvent::TextStarted { index: 0 },
    ];
    if fragmented {
        events.extend([
            ProviderStreamEvent::TextDelta {
                index: 0,
                delta: "hel".to_owned(),
            },
            ProviderStreamEvent::TextDelta {
                index: 0,
                delta: "lo".to_owned(),
            },
        ]);
    } else {
        events.push(ProviderStreamEvent::TextDelta {
            index: 0,
            delta: "hello".to_owned(),
        });
    }
    events.extend([
        ProviderStreamEvent::TextEnded { index: 0 },
        ProviderStreamEvent::ReasoningStarted {
            index: 1,
            summary_index: 0,
        },
    ]);
    if fragmented {
        events.extend([
            ProviderStreamEvent::ReasoningSummaryDelta {
                index: 1,
                summary_index: 0,
                delta: "plan ".to_owned(),
            },
            ProviderStreamEvent::ReasoningSummaryDelta {
                index: 1,
                summary_index: 0,
                delta: "done".to_owned(),
            },
        ]);
    } else {
        events.push(ProviderStreamEvent::ReasoningSummaryDelta {
            index: 1,
            summary_index: 0,
            delta: "plan done".to_owned(),
        });
    }
    events.extend([
        ProviderStreamEvent::ReasoningContentDelta {
            index: 1,
            content_index: 0,
            delta: "hidden reasoning".to_owned(),
        },
        ProviderStreamEvent::ReasoningEnded { index: 1 },
        ProviderStreamEvent::Usage(ProviderTokenUsage {
            input_tokens: 10,
            cached_input_tokens: 2,
            cache_write_input_tokens: 1,
            output_tokens: 4,
            reasoning_output_tokens: 2,
        }),
        ProviderStreamEvent::Finished(ProviderFinishReason::Stop),
    ]);
    events
}

#[test]
fn fragmentation_replay_reasoning_usage_and_sequence_are_deterministic() {
    let receipt = gateway_receipt();
    let fragmented = convert(&receipt, positive_events(true));
    let coalesced = convert(&receipt, positive_events(false));
    let replay = convert(&receipt, positive_events(true));
    assert_eq!(fragmented, coalesced);
    assert_eq!(fragmented, replay);
    assert_eq!(
        fragmented
            .iter()
            .map(CanonicalModelStreamFrame::sequence)
            .collect::<Vec<_>>(),
        (1..=u64::try_from(fragmented.len()).expect("frame count")).collect::<Vec<_>>()
    );
    assert!(
        fragmented
            .last()
            .is_some_and(CanonicalModelStreamFrame::is_terminal)
    );
    assert!(
        fragmented[..fragmented.len() - 1]
            .iter()
            .all(|frame| !frame.is_terminal())
    );

    let payloads = values(&fragmented);
    assert_eq!(payloads[0]["type"], "created");
    assert_eq!(payloads[1]["type"], "server_model");
    assert!(
        payloads
            .iter()
            .any(|value| { value["type"] == "output_text_delta" && value["delta"] == "hello" })
    );
    assert!(payloads.iter().any(|value| {
        value["type"] == "reasoning_summary_delta" && value["delta"] == "plan done"
    }));
    assert!(payloads.iter().any(|value| {
        value["type"] == "reasoning_content_delta" && value["delta"] == "hidden reasoning"
    }));
    let completed = payloads.last().expect("terminal payload");
    assert_eq!(completed["type"], "completed");
    assert_eq!(completed["endTurn"], true);
    assert_eq!(completed["tokenUsage"]["total_tokens"], 14);
    assert_eq!(completed["tokenUsage"]["reasoning_output_tokens"], 2);

    let encoded = fragmented[0].encoded_payload();
    assert_eq!(encoded.content_type, "application/json");
    assert_eq!(
        STANDARD.decode(encoded.data_base64).expect("decode frame"),
        fragmented[0].payload_json().as_bytes()
    );
    assert_eq!(
        encoded.payload_digest.0,
        format!(
            "sha256:{:x}",
            Sha256::digest(fragmented[0].payload_json().as_bytes())
        )
    );
    assert!(!format!("{:?}", fragmented[4]).contains("hello"));
}

fn parallel_tool_events(fragmented: bool) -> Vec<ProviderStreamEvent> {
    let mut events = vec![
        ProviderStreamEvent::ResponseStarted {
            provider_response_id: "response-tools".to_owned(),
        },
        ProviderStreamEvent::ToolCallStarted {
            index: 10,
            provider_call_id: "call-alpha".to_owned(),
            identity: ProviderToolIdentity::try_new(
                ProviderToolKind::Function,
                "read_file".to_owned(),
                Some("workspace".to_owned()),
            )
            .expect("workspace function identity"),
        },
        ProviderStreamEvent::ToolCallStarted {
            index: 11,
            provider_call_id: "call-beta".to_owned(),
            identity: ProviderToolIdentity::try_new(
                ProviderToolKind::Custom,
                "write_note".to_owned(),
                Some("project".to_owned()),
            )
            .expect("project custom identity"),
        },
    ];
    if fragmented {
        events.extend([
            ProviderStreamEvent::ToolCallArgumentsDelta {
                index: 10,
                provider_call_id: "call-alpha".to_owned(),
                delta: "{\"path\":".to_owned(),
            },
            ProviderStreamEvent::ToolCallArgumentsDelta {
                index: 11,
                provider_call_id: "call-beta".to_owned(),
                delta: "note".to_owned(),
            },
            ProviderStreamEvent::ToolCallArgumentsDelta {
                index: 10,
                provider_call_id: "call-alpha".to_owned(),
                delta: "\"README.md\"}".to_owned(),
            },
        ]);
    } else {
        events.extend([
            ProviderStreamEvent::ToolCallArgumentsDelta {
                index: 10,
                provider_call_id: "call-alpha".to_owned(),
                delta: "{\"path\":\"README.md\"}".to_owned(),
            },
            ProviderStreamEvent::ToolCallArgumentsDelta {
                index: 11,
                provider_call_id: "call-beta".to_owned(),
                delta: "note".to_owned(),
            },
        ]);
    }
    events.extend([
        ProviderStreamEvent::ToolCallEnded {
            index: 11,
            provider_call_id: "call-beta".to_owned(),
        },
        ProviderStreamEvent::ToolCallEnded {
            index: 10,
            provider_call_id: "call-alpha".to_owned(),
        },
        ProviderStreamEvent::Finished(ProviderFinishReason::ToolCalls),
    ]);
    events
}

#[test]
fn parallel_tool_identities_are_stable_and_identity_drift_is_terminal() {
    let receipt = gateway_receipt();
    let fragmented = convert(&receipt, parallel_tool_events(true));
    let coalesced = convert(&receipt, parallel_tool_events(false));
    assert_eq!(fragmented, coalesced);
    let payloads = values(&fragmented);
    let added = payloads
        .iter()
        .filter(|value| value["type"] == "output_item_added")
        .collect::<Vec<_>>();
    assert_eq!(added.len(), 2);
    assert_eq!(added[0]["item"]["call_id"], "call-alpha");
    assert_eq!(added[1]["item"]["call_id"], "call-beta");
    assert_eq!(added[0]["item"]["namespace"], "workspace");
    assert_eq!(added[1]["item"]["namespace"], "project");
    assert_ne!(added[0]["item"]["id"], added[1]["item"]["id"]);
    assert_eq!(payloads.last().expect("terminal")["endTurn"], false);

    let mut converter = ProviderStreamConverter::from_gateway_receipt(&receipt);
    converter
        .ingest(ProviderStreamEvent::ResponseStarted {
            provider_response_id: "response-drift".to_owned(),
        })
        .expect("start drift fixture");
    converter
        .ingest(ProviderStreamEvent::ToolCallStarted {
            index: 5,
            provider_call_id: "call-original".to_owned(),
            identity: ProviderToolIdentity::try_new(
                ProviderToolKind::Function,
                "read_file".to_owned(),
                Some("workspace".to_owned()),
            )
            .expect("drift fixture identity"),
        })
        .expect("start tool call");
    let drift = converter
        .ingest(ProviderStreamEvent::ToolCallArgumentsDelta {
            index: 5,
            provider_call_id: "call-replaced".to_owned(),
            delta: "{}".to_owned(),
        })
        .expect_err("Provider cannot change parallel call identity");
    assert_eq!(drift.kind(), ProviderStreamConversionErrorKind::Protocol);
    let after_terminal = converter
        .ingest(ProviderStreamEvent::Disconnected)
        .expect_err("protocol failure is terminal");
    assert_eq!(
        after_terminal.kind(),
        ProviderStreamConversionErrorKind::AlreadyTerminal
    );
}

#[test]
fn cancellation_disconnect_failure_and_split_secret_are_fail_closed() {
    let receipt = gateway_receipt();

    let mut leaking = ProviderStreamConverter::from_gateway_receipt(&receipt);
    leaking
        .ingest(ProviderStreamEvent::ResponseStarted {
            provider_response_id: "response-leak".to_owned(),
        })
        .expect("start leak fixture");
    leaking
        .ingest(ProviderStreamEvent::TextStarted { index: 0 })
        .expect("start leak text");
    let split = PROVIDER_SECRET.len() / 2;
    let first = String::from_utf8(PROVIDER_SECRET[..split].to_vec()).expect("secret UTF-8");
    let second = String::from_utf8(PROVIDER_SECRET[split..].to_vec()).expect("secret UTF-8");
    assert!(
        leaking
            .ingest(ProviderStreamEvent::TextDelta {
                index: 0,
                delta: first,
            })
            .expect("first half is buffered")
            .is_empty()
    );
    let leak = leaking
        .ingest(ProviderStreamEvent::TextDelta {
            index: 0,
            delta: second,
        })
        .expect_err("complete split Credential is rejected before emission");
    assert_eq!(
        leak.kind(),
        ProviderStreamConversionErrorKind::CredentialLeak
    );
    assert!(!format!("{leak:?}").contains("provider-stream-secret"));

    for (event, code) in [
        (ProviderStreamEvent::Cancelled, "CANCELLED"),
        (ProviderStreamEvent::Disconnected, "STREAM_CLOSED"),
    ] {
        let mut converter = ProviderStreamConverter::from_gateway_receipt(&receipt);
        converter
            .ingest(ProviderStreamEvent::ResponseStarted {
                provider_response_id: format!("response-{code}"),
            })
            .expect("start terminal fixture");
        converter
            .ingest(ProviderStreamEvent::TextStarted { index: 0 })
            .expect("leave one block open");
        let frames = converter.ingest(event).expect("explicit terminal event");
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_terminal());
        let value: Value = serde_json::from_str(frames[0].payload_json()).expect("terminal JSON");
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["code"], code);
    }

    let mut failed = ProviderStreamConverter::from_gateway_receipt(&receipt);
    failed
        .ingest(ProviderStreamEvent::ResponseStarted {
            provider_response_id: "response-failed".to_owned(),
        })
        .expect("start failure fixture");
    let frames = failed
        .ingest(ProviderStreamEvent::Failed(
            ProviderStreamFailure::new(ProviderStreamFailureKind::RateLimit)
                .with_status(429)
                .with_retry_after_millis(500)
                .with_provider_request_id("provider-request-safe".to_owned()),
        ))
        .expect("map stable Provider failure");
    let failure: Value = serde_json::from_str(frames[0].payload_json()).expect("failure JSON");
    assert_eq!(failure["error"]["code"], "RATE_LIMIT");
    assert_eq!(failure["error"]["status"], 429);
    assert_eq!(failure["error"]["providerRetryAfterMillis"], 500);
    assert_eq!(
        failure["error"]["providerRequestId"],
        "sha256:b39c43372e7181a79a28eeb0e5259c53e1f9f743febc65005d7b39376179c18d"
    );
}

#[test]
fn max_tokens_is_a_terminal_error_and_debug_never_renders_provider_content() {
    let receipt = gateway_receipt();
    let frames = convert(
        &receipt,
        vec![
            ProviderStreamEvent::ResponseStarted {
                provider_response_id: "response-max-tokens".to_owned(),
            },
            ProviderStreamEvent::Finished(ProviderFinishReason::MaxTokens),
        ],
    );
    let terminal = values(&frames).pop().expect("max-token terminal frame");
    assert_eq!(terminal["type"], "error");
    assert_eq!(terminal["error"]["code"], "MAX_TOKENS");
    assert!(
        frames
            .last()
            .is_some_and(CanonicalModelStreamFrame::is_terminal)
    );

    let event = ProviderStreamEvent::TextDelta {
        index: 0,
        delta: String::from_utf8(PROVIDER_SECRET.to_vec()).expect("secret UTF-8"),
    };
    assert!(!format!("{event:?}").contains("provider-stream-secret"));
    let failure = ProviderStreamFailure::new(ProviderStreamFailureKind::Server)
        .with_provider_request_id("sensitive-provider-request-id".to_owned());
    assert!(!format!("{failure:?}").contains("sensitive-provider-request-id"));
}

#[test]
fn empty_stop_matches_dsh_retryable_failure_and_terminal_is_exact_once() {
    let receipt = gateway_receipt();
    let mut converter = ProviderStreamConverter::from_gateway_receipt(&receipt);
    converter
        .ingest(ProviderStreamEvent::ResponseStarted {
            provider_response_id: "response-empty".to_owned(),
        })
        .expect("start empty response");
    let frames = converter
        .ingest(ProviderStreamEvent::Finished(ProviderFinishReason::Stop))
        .expect("map empty stop to a typed failure");
    assert_eq!(frames.len(), 1);
    assert!(frames[0].is_terminal());
    let terminal: Value =
        serde_json::from_str(frames[0].payload_json()).expect("empty terminal JSON");
    assert_eq!(terminal["type"], "error");
    assert_eq!(terminal["error"]["code"], "EMPTY_RESPONSE");
    assert_eq!(
        converter
            .ingest(ProviderStreamEvent::Finished(ProviderFinishReason::Stop))
            .expect_err("terminal response cannot be emitted twice")
            .kind(),
        ProviderStreamConversionErrorKind::AlreadyTerminal
    );
}
