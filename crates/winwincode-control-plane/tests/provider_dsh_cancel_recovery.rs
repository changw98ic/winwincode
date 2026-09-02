// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeMap,
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
    CredentialReferenceCreatePayload, CredentialReferenceRevokeCommand,
    CredentialReferenceRevokeCommandCommand, CredentialReferenceRevokePayload, ModelRoute,
    OrganizationScope, OrganizationScopeKind, RepositoryScope, RepositoryScopeKind, Scope,
    UserActor, UserActorKind,
};
use winwincode_control_plane::{
    CredentialReferenceResolution, CredentialReferenceService, DurableModelExchangeAuthority,
    FrozenModelRouteAuthority, ModelAttemptFailureFact, ModelAttemptFailureKind, ModelCapability,
    ModelExecutionCertainty, ModelReservationReceipt, ModelReservationReleaseReason,
    ModelReservationTerminalOutcome, ModelReservationTerminalReceipt, ModelSettingsRequest,
    ModelSettingsService, ModelSettingsTarget, ModelSettingsValues, ModelToolSupport,
    ProviderAdapterError, ProviderAdapterInvocation, ProviderAdapterOpenReceipt,
    ProviderAdapterPort, ProviderAdmissionError, ProviderAdmissionOpenReceipt,
    ProviderAdmissionOpenRequest, ProviderCatalogRequest, ProviderCatalogService,
    ProviderDescriptor, ProviderGateway, ProviderGatewayAdmissionPort,
    ProviderGatewayDurableExchange, ProviderGatewayError, ProviderGatewayErrorKind,
    ProviderGatewayIdentity, ProviderGatewayIdentityError, ProviderGatewayIdentityPort,
    ProviderGatewayOpenReceipt, ProviderGatewaySettlement, ProviderGatewaySettlementError,
    ProviderGatewaySettlementPort, ProviderGatewayTerminal, ProviderGatewayTerminalOutcome,
    ProviderGatewayTerminalReceipt, ProviderStreamControlAction, ProviderStreamConverter,
    ProviderStreamEvent, ProviderStreamFailure, ProviderStreamFailureKind, ProviderTokenUsage,
    ResolvedSecret, SecretStoreError, SecretStorePort,
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
use winwincode_storage::{ProviderExchangeBegin, ProviderExchangeOpened, SqliteStorage};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);
const DIFFERENTIAL_FIXTURE: &str =
    include_str!("../../../tests/fixtures/provider-dsh-rust-differential.v1.json");

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-provider-dsh-cancel-{name}-{}-{suffix}",
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
        model_id: "model-differential".to_owned(),
        provider_id: "provider-differential".to_owned(),
    }
}

fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor {
        provider_id: "provider-differential".to_owned(),
        display_name: "Differential Provider".to_owned(),
        adapter_kind: "fixture-adapter".to_owned(),
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
        models: vec![ModelCapability {
            model_id: "model-differential".to_owned(),
            display_name: "Differential model".to_owned(),
            context_window_tokens: 128_000,
            max_output_tokens: 16_000,
            tool_support: ModelToolSupport::Parallel,
            reasoning_efforts: vec!["high".to_owned()],
        }],
    }
}

fn configure(storage: &mut SqliteStorage) {
    ProviderCatalogService::new(storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: actor(),
                scope: Scope::OrganizationScope(organization_scope()),
                request_id: RequestId(id("req", 1)),
                expected_catalog_version: 0,
            },
            &descriptor(),
        )
        .expect("create Provider catalog");
    CredentialReferenceService::new(storage)
        .create(
            &CredentialReferenceCreateCommand {
                actor: actor(),
                command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
                expected_revision: Revision(0),
                payload: CredentialReferenceCreatePayload {
                    credential_reference_id: CredentialReferenceId(id("crd", 1)),
                    display_name: "Differential credential".to_owned(),
                    provider_id: "provider-differential".to_owned(),
                    vault_locator: "local-fixture://differential".to_owned(),
                },
                request_id: RequestId(id("req", 2)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::OrganizationScope(organization_scope()),
            },
            1_800_000_000_000,
        )
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
            route: "differential-route".to_owned(),
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

fn cancellation_ack(message: &ModelOpenMessage, seed: u64) -> ModelAckMessage {
    ModelAckMessage {
        ack_sequence: ExecutionAckSequence(0),
        error: Some(ExecutionPortError {
            code: ExecutionPortErrorCode::Cancelled,
            message: "model exchange cancelled by Worker".to_owned(),
            retryable: false,
        }),
        kind: ModelAckMessageKind::ModelAck,
        lease: message.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", seed)),
        model_exchange_id: message.model_exchange_id.clone(),
        replay_from_sequence: None,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2030-01-01T00:00:02Z".to_owned()),
        session_identity: message.session_identity.clone(),
        status: LeaseWriteStatus::RejectedConflict,
        worker_session_id: message.worker_session_id.clone(),
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
struct SecretStoreProbe {
    resolutions: AtomicU64,
}

impl SecretStorePort for SecretStoreProbe {
    fn resolve(
        &self,
        _reference: &CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError> {
        self.resolutions.fetch_add(1, Ordering::Relaxed);
        ResolvedSecret::from_bytes(b"differential-secret".to_vec())
    }
}

#[derive(Default)]
struct AdapterState {
    opens: AtomicU64,
    controls: Mutex<Vec<(ModelExchangeId, ProviderStreamControlAction)>>,
}

#[derive(Clone, Default)]
struct AdapterProbe(Arc<AdapterState>);

impl ProviderAdapterPort for AdapterProbe {
    fn provider_id(&self) -> &'static str {
        "provider-differential"
    }

    fn open(
        &self,
        invocation: &ProviderAdapterInvocation<'_>,
        credential: &ResolvedSecret,
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError> {
        assert_eq!(credential.expose(), b"differential-secret");
        self.0.opens.fetch_add(1, Ordering::Relaxed);
        ProviderAdapterOpenReceipt::try_new(invocation.adapter_request_id().to_owned())
    }

    fn control(
        &self,
        model_exchange_id: &ModelExchangeId,
        _adapter_request_id: &str,
        action: ProviderStreamControlAction,
    ) -> Result<(), ProviderAdapterError> {
        self.0
            .controls
            .lock()
            .expect("lock adapter controls")
            .push((model_exchange_id.clone(), action));
        Ok(())
    }
}

#[derive(Default)]
struct SettlementProbe {
    accepted: Mutex<Vec<ProviderGatewaySettlement>>,
}

impl ProviderGatewaySettlementPort for SettlementProbe {
    fn settle(
        &self,
        settlement: &ProviderGatewaySettlement,
    ) -> Result<(), ProviderGatewaySettlementError> {
        self.accepted
            .lock()
            .expect("lock settlements")
            .push(settlement.clone());
        Ok(())
    }
}

#[derive(Default)]
struct AdmissionProbe {
    reserves: AtomicU64,
    releases: AtomicU64,
    completes: AtomicU64,
    reserved: Mutex<BTreeMap<String, ProviderAdmissionOpenReceipt>>,
}

impl ProviderGatewayAdmissionPort for AdmissionProbe {
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
        let revision = self.reserves.fetch_add(1, Ordering::Relaxed) + 1;
        let receipt = ProviderAdmissionOpenReceipt {
            reservation: ModelReservationReceipt {
                request_id: request.message.request_id.clone(),
                model_exchange_id: request.message.model_exchange_id.clone(),
                route_authority_fingerprint: authority.fingerprint().to_owned(),
                denial: None,
                unix_minute: 1,
                revision,
                idempotent_replay: false,
            },
            route_authority: authority,
            enterprise_quota_amounts: winwincode_storage::EnterpriseQuotaAmounts {
                tokens: 100,
                provider_cost_micros: 10,
                operations: 1,
                ..winwincode_storage::EnterpriseQuotaAmounts::default()
            },
        };
        self.reserved
            .lock()
            .expect("lock reservations")
            .insert(request.message.model_exchange_id.0.clone(), receipt.clone());
        Ok(receipt)
    }

    fn release(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        reason: ModelReservationReleaseReason,
    ) -> Result<ModelReservationTerminalReceipt, ProviderAdmissionError> {
        self.reserved
            .lock()
            .expect("lock reservations")
            .remove(&model_exchange_id.0);
        let revision = self.releases.fetch_add(1, Ordering::Relaxed) + 1;
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
            revision,
            idempotent_replay: false,
        })
    }

    fn release_if_reserved(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        reason: ModelReservationReleaseReason,
    ) -> Result<Option<ModelReservationTerminalReceipt>, ProviderAdmissionError> {
        if !self
            .reserved
            .lock()
            .expect("lock reservations")
            .contains_key(&model_exchange_id.0)
        {
            return Ok(None);
        }
        self.release(authority, original_request_id, model_exchange_id, reason)
            .map(Some)
    }

    fn complete(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        usage: ProviderTokenUsage,
        actual_cost_micros: u64,
    ) -> Result<ModelReservationTerminalReceipt, ProviderAdmissionError> {
        self.reserved
            .lock()
            .expect("lock reservations")
            .remove(&model_exchange_id.0);
        let revision = self.completes.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(ModelReservationTerminalReceipt {
            request_id: original_request_id.clone(),
            model_exchange_id: model_exchange_id.clone(),
            route_authority_fingerprint: authority.fingerprint().to_owned(),
            outcome: ModelReservationTerminalOutcome::Completed,
            actual_tokens: usage.input_tokens + usage.output_tokens,
            actual_cost_micros,
            revision,
            idempotent_replay: false,
        })
    }
}

struct Harness {
    root: PathBuf,
    storage: SqliteStorage,
    secret_store: SecretStoreProbe,
    identity: Identity,
    settlement: SettlementProbe,
    admission: AdmissionProbe,
    adapter: AdapterProbe,
}

impl Harness {
    fn new(name: &str) -> Self {
        let root = temporary_directory(name);
        let mut storage = SqliteStorage::open(&root).expect("open fixture storage");
        configure(&mut storage);
        Self {
            root,
            storage,
            secret_store: SecretStoreProbe::default(),
            identity: Identity,
            settlement: SettlementProbe::default(),
            admission: AdmissionProbe::default(),
            adapter: AdapterProbe::default(),
        }
    }

    fn open(&mut self, seed: u64) -> ProviderGatewayDurableExchange {
        let message = open_message(seed);
        let durable = {
            let mut gateway = ProviderGateway::new(
                &mut self.storage,
                &self.secret_store,
                &self.identity,
                &self.settlement,
                &mut self.admission,
            );
            gateway
                .register_adapter(Box::new(self.adapter.clone()))
                .expect("register adapter");
            let receipt = gateway
                .open(&message, &route(), &format!("adapter-differential-{seed}"))
                .expect("open Provider exchange");
            gateway
                .durable_exchange(&receipt.model_exchange_id)
                .expect("snapshot Provider exchange")
        };
        seed_durable_exchange(&mut self.storage, &message, &durable);
        durable
    }

    fn apply(
        &mut self,
        durable: &ProviderGatewayDurableExchange,
        command: ProviderGatewayTerminal,
        progress: &DurableModelExchangeAuthority,
    ) -> Result<ProviderGatewayTerminalReceipt, ProviderGatewayError> {
        let mut gateway = ProviderGateway::new(
            &mut self.storage,
            &self.secret_store,
            &self.identity,
            &self.settlement,
            &mut self.admission,
        );
        gateway
            .register_adapter(Box::new(self.adapter.clone()))
            .expect("register adapter");
        gateway
            .restore_durable_exchange(durable)
            .expect("restore durable exchange");
        gateway.apply_terminal_with_progress(
            &durable.open_receipt().model_exchange_id,
            command,
            progress,
            &Instant("2030-01-01T00:00:03Z".to_owned()),
        )
    }

    fn cancel(
        &mut self,
        seed: u64,
        durable: &ProviderGatewayDurableExchange,
        progress: &DurableModelExchangeAuthority,
    ) -> Result<ProviderGatewayTerminalReceipt, ProviderGatewayError> {
        let mut gateway = ProviderGateway::new(
            &mut self.storage,
            &self.secret_store,
            &self.identity,
            &self.settlement,
            &mut self.admission,
        );
        gateway
            .register_adapter(Box::new(self.adapter.clone()))
            .expect("register adapter");
        gateway
            .restore_durable_exchange(durable)
            .expect("restore durable exchange");
        gateway.cancel_from_worker_with_progress(
            &cancellation_ack(&open_message(seed), 900 + seed),
            progress,
        )
    }

    fn finish(self) {
        drop(self.storage);
        fs::remove_dir_all(self.root).expect("remove fixture");
    }
}

fn seed_durable_exchange(
    storage: &mut SqliteStorage,
    message: &ModelOpenMessage,
    durable: &ProviderGatewayDurableExchange,
) {
    let receipt_json = durable
        .to_durable_receipt_json()
        .expect("encode durable receipt");
    let receipt: Value = serde_json::from_slice(&receipt_json).expect("parse durable receipt");
    let open_digest = Sha256Digest(
        receipt["gatewayOpenDigest"]
            .as_str()
            .expect("Gateway open digest")
            .to_owned(),
    );
    let mut store = storage
        .provider_exchange_store()
        .expect("open Provider exchange store");
    store
        .begin_open(&ProviderExchangeBegin {
            model_exchange_id: message.model_exchange_id.clone(),
            request_id: message.request_id.clone(),
            message_id: message.message_id.clone(),
            open_digest: open_digest.clone(),
            provider_id: durable.open_receipt().route.provider_id.clone(),
            adapter_request_id: durable.open_receipt().adapter_request_id.clone(),
            started_at: message.sent_at.clone(),
        })
        .expect("begin durable exchange");
    let opened = ProviderExchangeOpened::new(
        Sha256Digest(durable.route_authority().fingerprint().to_owned()),
        durable
            .route_authority()
            .to_durable_json()
            .expect("encode route authority"),
        receipt_json,
        b"{}".to_vec(),
        message.sent_at.clone(),
    )
    .expect("build opened exchange");
    store
        .commit_opened(&message.model_exchange_id, &open_digest, &opened)
        .expect("commit opened exchange");
}

const fn usage() -> ProviderTokenUsage {
    ProviderTokenUsage {
        input_tokens: 10,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 5,
        reasoning_output_tokens: 0,
    }
}

fn completed() -> ProviderGatewayTerminal {
    ProviderGatewayTerminal::Completed {
        usage: usage(),
        actual_cost_micros: 25,
    }
}

fn failed(kind: ModelAttemptFailureKind) -> ProviderGatewayTerminal {
    ProviderGatewayTerminal::Failed {
        failure: ModelAttemptFailureFact {
            kind,
            certainty: ModelExecutionCertainty::AcceptanceUnknown,
        },
        charge: None,
    }
}

fn revoke_credential() -> CredentialReferenceRevokeCommand {
    CredentialReferenceRevokeCommand {
        actor: actor(),
        command: CredentialReferenceRevokeCommandCommand::CredentialReferenceRevoke,
        expected_revision: Revision(1),
        payload: CredentialReferenceRevokePayload {
            credential_reference_id: CredentialReferenceId(id("crd", 1)),
        },
        request_id: RequestId(id("req", 800)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::OrganizationScope(organization_scope()),
    }
}

fn terminal_frame(receipt: &ProviderGatewayOpenReceipt, event: ProviderStreamEvent) -> String {
    let mut converter = ProviderStreamConverter::from_gateway_receipt(receipt);
    converter
        .ingest(ProviderStreamEvent::ResponseStarted {
            provider_response_id: "response-differential".to_owned(),
        })
        .expect("start Provider response");
    converter
        .ingest(event)
        .expect("convert Provider terminal")
        .last()
        .expect("terminal frame")
        .payload_json()
        .to_owned()
}

fn differential_scenario(id: &str) -> Value {
    serde_json::from_str::<Value>(DIFFERENTIAL_FIXTURE)
        .expect("parse Provider differential fixture")["scenarios"]
        .as_array()
        .expect("fixture scenarios")
        .iter()
        .find(|scenario| scenario["id"] == id)
        .unwrap_or_else(|| panic!("missing canonical scenario {id}"))
        .clone()
}

fn differential_failure(code: &str) -> Value {
    differential_scenario("typed-errors")["input"]["failures"]
        .as_array()
        .expect("typed failure inputs")
        .iter()
        .find(|failure| failure["code"] == code)
        .unwrap_or_else(|| panic!("missing canonical failure {code}"))
        .clone()
}

#[test]
fn cancel_and_provider_finish_race_linearizes_to_one_terminal_and_one_release() {
    let scenario = differential_scenario("cancel-and-disconnect");
    assert_eq!(
        scenario["input"]["cancelFinishOrderings"],
        serde_json::json!(["cancel-before-finish", "finish-before-cancel"])
    );
    assert_eq!(
        scenario["input"]["terminalCodes"],
        serde_json::json!(["CANCELLED", "STREAM_CLOSED"])
    );
    let mut harness = Harness::new("cancel-finish-race");
    let cancelled = harness.open(1);
    let completed_exchange = harness.open(2);
    assert_eq!(
        serde_json::from_str::<Value>(&terminal_frame(
            cancelled.open_receipt(),
            ProviderStreamEvent::Cancelled
        ))
        .expect("parse cancellation terminal")["error"]["code"],
        "CANCELLED"
    );
    let progress = DurableModelExchangeAuthority::open(&harness.root).expect("open progress");

    let cancel_receipt = harness
        .cancel(1, &cancelled, &progress)
        .expect("cancel wins first ordering");
    assert_eq!(
        cancel_receipt.outcome,
        ProviderGatewayTerminalOutcome::Cancelled
    );
    assert_eq!(
        harness
            .apply(&cancelled, completed(), &progress)
            .expect_err("finish conflicts after cancel")
            .kind(),
        ProviderGatewayErrorKind::TerminalConflict
    );

    let completed_receipt = harness
        .apply(&completed_exchange, completed(), &progress)
        .expect("finish wins second ordering");
    assert_eq!(
        completed_receipt.outcome,
        ProviderGatewayTerminalOutcome::Succeeded
    );
    assert_eq!(
        harness
            .cancel(2, &completed_exchange, &progress)
            .expect_err("cancel conflicts after finish")
            .kind(),
        ProviderGatewayErrorKind::TerminalConflict
    );
    assert_eq!(harness.admission.releases.load(Ordering::Relaxed), 1);
    assert_eq!(harness.admission.completes.load(Ordering::Relaxed), 1);
    assert_eq!(
        harness
            .settlement
            .accepted
            .lock()
            .expect("lock settlements")
            .len(),
        2
    );
    let controls = harness
        .adapter
        .0
        .controls
        .lock()
        .expect("lock controls")
        .clone();
    assert_eq!(controls.len(), 3);
    assert_eq!(controls[0].1, ProviderStreamControlAction::Cancel);
    assert_eq!(controls[1].1, ProviderStreamControlAction::Release);
    assert_eq!(controls[2].1, ProviderStreamControlAction::Release);
    progress.close().expect("close progress");
    harness.finish();
}

#[test]
fn disconnect_settlement_restarts_as_exact_replay_without_second_adapter_call() {
    let scenario = differential_scenario("cancel-and-disconnect");
    assert_eq!(scenario["input"]["restartAfterDisconnect"], true);
    assert!(
        scenario["expectedFacts"]
            .as_array()
            .expect("fixture expected facts")
            .iter()
            .any(|fact| fact == "terminal replay cannot emit a second stream terminal")
    );
    let mut harness = Harness::new("disconnect-restart");
    let durable = harness.open(3);
    let terminal_json = terminal_frame(durable.open_receipt(), ProviderStreamEvent::Disconnected);
    assert_eq!(
        serde_json::from_str::<Value>(&terminal_json).expect("parse terminal")["error"]["code"],
        "STREAM_CLOSED"
    );
    let progress = DurableModelExchangeAuthority::open(&harness.root).expect("open progress");
    let first = harness
        .apply(
            &durable,
            failed(ModelAttemptFailureKind::Transport),
            &progress,
        )
        .expect("settle disconnected exchange");
    assert!(!first.idempotent_replay);
    progress.close().expect("close first progress");

    let restarted = DurableModelExchangeAuthority::open(&harness.root).expect("restart progress");
    let replay = harness
        .apply(
            &durable,
            failed(ModelAttemptFailureKind::Transport),
            &restarted,
        )
        .expect("replay disconnected settlement");
    assert!(replay.idempotent_replay);
    assert_eq!(
        terminal_frame(durable.open_receipt(), ProviderStreamEvent::Disconnected),
        terminal_json
    );
    assert_eq!(harness.secret_store.resolutions.load(Ordering::Relaxed), 1);
    assert_eq!(harness.adapter.0.opens.load(Ordering::Relaxed), 1);
    assert_eq!(harness.admission.releases.load(Ordering::Relaxed), 1);
    assert_eq!(
        harness
            .settlement
            .accepted
            .lock()
            .expect("lock settlements")
            .len(),
        1
    );
    assert_eq!(
        harness
            .adapter
            .0
            .controls
            .lock()
            .expect("lock controls")
            .len(),
        2
    );
    restarted.close().expect("close restarted progress");
    harness.finish();
}

#[test]
fn rate_limit_and_timeout_keep_status_retry_and_exact_terminal_replay() {
    let rate_fixture = differential_failure("RATE_LIMIT");
    let timeout_fixture = differential_failure("TIMEOUT");
    assert_eq!(rate_fixture["status"], 429);
    assert_eq!(rate_fixture["retryAfterMillis"], 750);
    assert_eq!(timeout_fixture["status"], 504);
    assert!(timeout_fixture.get("retryAfterMillis").is_none());
    let mut harness = Harness::new("rate-timeout-replay");
    let rate_limited = harness.open(4);
    let timed_out = harness.open(5);
    let rate_event = ProviderStreamEvent::Failed(
        ProviderStreamFailure::new(ProviderStreamFailureKind::RateLimit)
            .with_status(429)
            .with_retry_after_millis(750),
    );
    let timeout_event = ProviderStreamEvent::Failed(
        ProviderStreamFailure::new(ProviderStreamFailureKind::Timeout).with_status(504),
    );
    let rate_json = terminal_frame(rate_limited.open_receipt(), rate_event.clone());
    let timeout_json = terminal_frame(timed_out.open_receipt(), timeout_event.clone());
    assert_failure_projection(&rate_json, "RATE_LIMIT", 429, Some(750));
    assert_failure_projection(&timeout_json, "TIMEOUT", 504, None);

    let progress = DurableModelExchangeAuthority::open(&harness.root).expect("open progress");
    harness
        .apply(
            &rate_limited,
            failed(ModelAttemptFailureKind::RateLimit),
            &progress,
        )
        .expect("settle rate limit");
    harness
        .apply(
            &timed_out,
            failed(ModelAttemptFailureKind::Timeout),
            &progress,
        )
        .expect("settle timeout");
    progress.close().expect("close progress");

    let restarted = DurableModelExchangeAuthority::open(&harness.root).expect("restart progress");
    let rate_replay = harness
        .apply(
            &rate_limited,
            failed(ModelAttemptFailureKind::RateLimit),
            &restarted,
        )
        .expect("replay rate limit");
    let timeout_replay = harness
        .apply(
            &timed_out,
            failed(ModelAttemptFailureKind::Timeout),
            &restarted,
        )
        .expect("replay timeout");
    assert!(rate_replay.idempotent_replay && timeout_replay.idempotent_replay);
    assert_eq!(
        terminal_frame(rate_limited.open_receipt(), rate_event),
        rate_json
    );
    assert_eq!(
        terminal_frame(timed_out.open_receipt(), timeout_event),
        timeout_json
    );
    assert_eq!(harness.secret_store.resolutions.load(Ordering::Relaxed), 2);
    assert_eq!(harness.adapter.0.opens.load(Ordering::Relaxed), 2);
    assert_eq!(harness.admission.releases.load(Ordering::Relaxed), 2);
    assert_eq!(
        harness
            .settlement
            .accepted
            .lock()
            .expect("lock settlements")
            .len(),
        2
    );
    restarted.close().expect("close restarted progress");
    harness.finish();
}

fn assert_failure_projection(payload: &str, code: &str, status: u64, retry_after: Option<u64>) {
    let value: Value = serde_json::from_str(payload).expect("parse Provider terminal");
    assert_eq!(value["error"]["code"], code);
    assert_eq!(value["error"]["status"], status);
    match retry_after {
        Some(retry_after) => {
            assert_eq!(value["error"]["providerRetryAfterMillis"], retry_after);
        }
        None => assert!(value["error"].get("providerRetryAfterMillis").is_none()),
    }
}

#[test]
fn revoked_credential_preserves_frozen_open_and_blocks_next_before_secret_or_adapter() {
    let scenario = differential_scenario("credential-failure");
    assert!(
        scenario["expectedFacts"]
            .as_array()
            .expect("fixture expected facts")
            .iter()
            .any(|fact| fact == "revoked credential references fail before secret-store access")
    );
    let mut harness = Harness::new("credential-revoke-snapshot");
    let durable = harness.open(6);
    let authority_json = durable
        .route_authority()
        .to_durable_json()
        .expect("encode frozen authority");
    let receipt_json = durable
        .to_durable_receipt_json()
        .expect("encode frozen receipt");
    CredentialReferenceService::new(&mut harness.storage)
        .revoke(&revoke_credential(), 1_800_000_001_000)
        .expect("revoke Credential reference");

    let progress = DurableModelExchangeAuthority::open(&harness.root).expect("open progress");
    let completed = harness
        .apply(&durable, completed(), &progress)
        .expect("complete already-open frozen request");
    assert_eq!(completed.outcome, ProviderGatewayTerminalOutcome::Succeeded);
    assert_eq!(
        durable
            .route_authority()
            .to_durable_json()
            .expect("re-encode frozen authority"),
        authority_json
    );
    assert_eq!(
        durable
            .to_durable_receipt_json()
            .expect("re-encode frozen receipt"),
        receipt_json
    );

    let mut gateway = ProviderGateway::new(
        &mut harness.storage,
        &harness.secret_store,
        &harness.identity,
        &harness.settlement,
        &mut harness.admission,
    );
    gateway
        .register_adapter(Box::new(harness.adapter.clone()))
        .expect("register adapter");
    let rejected = gateway
        .open(&open_message(7), &route(), "adapter-differential-7")
        .expect_err("new open rejects revoked Credential");
    assert_eq!(
        rejected.kind(),
        ProviderGatewayErrorKind::CredentialUnavailable
    );
    drop(gateway);
    assert_eq!(harness.secret_store.resolutions.load(Ordering::Relaxed), 1);
    assert_eq!(harness.adapter.0.opens.load(Ordering::Relaxed), 1);
    assert_eq!(harness.admission.reserves.load(Ordering::Relaxed), 1);
    assert_eq!(harness.admission.completes.load(Ordering::Relaxed), 1);
    progress.close().expect("close progress");
    harness.finish();
}
