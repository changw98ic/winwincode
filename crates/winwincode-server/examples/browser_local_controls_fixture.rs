// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::signal::unix::{SignalKind, signal};
use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, ModelRoute, OrganizationScope, OrganizationScopeKind, Scope,
};
use winwincode_control_plane::{
    ChatInteractionService, ContinueProductSessionCommand, ControlPlane, ControlPlaneConfig,
    CredentialReferenceService, DurableWorkerInteractionOutbound, GateCandidateIdentity,
    GateDecisionFact, GateInteractionActor, GateInteractionAuthority,
    GateInteractionCommandContext, GateInteractionService, GateInteractionSubject, ModelCapability,
    ModelToolSupport, ProductSessionCommandContext, ProductSessionExecutionConfig,
    ProductSessionService, ProviderCatalogRequest, ProviderCatalogService, ProviderDescriptor,
    RecordApprovalInteractionCommand, RegisterGateInteractionCommand, RoutableGateDecision,
    StructuredOutputSupport,
};
use winwincode_domain::{
    ApprovalId, CodexThreadId, ControlPlaneEventId, CredentialReferenceId, ExecutionJobId,
    ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId, ModelExchangeId,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion,
    Sha256Digest, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_domain::{
    ControlPlaneWebSocketAuthorizationEpoch, RepositoryScope, RepositoryScopeKind, UserActor,
    UserActorKind,
};
use winwincode_execution_port::action_gateway::GateDecision;
use winwincode_execution_port::generated::ApprovalRequestMessage;
use winwincode_server::{
    AuthSessionBootstrap, AuthSessionConfig, AuthenticatedPrincipal, DurableEventHub,
    DurableEventHubConfig, DurableEventPublisher, GeneratedContractDispatcher,
    RequestAuthenticator, ServerConfig, ServerTls, SqliteAuthSessionManager,
    StandaloneControlPlaneApplication, start_server,
};
use winwincode_session::SessionBindingIdentity;
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionLeaseClaim, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, LeaseWriteStatus, ProductStateStorage,
    PublicEventActor, PublicEventScope, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey,
    SqliteStorage, WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerOutboundQueueConfig,
    WorkerPlatform, WorkerPoolId, WorkerRegistrationRequest, WorkerSlotAuthority,
    WorkerSlotOpenRequest, WorkerSlotResourceLimits, WorkerSlotResources,
};

const FIXTURE_SEED: u64 = 1;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("browser-local-controls fixture: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let config = environment_config()?;
    let actor = actor();
    let scope = repository_scope();
    seed_once(config.data_directory())?;

    let hub = Arc::new(DurableEventHub::open(
        config.data_directory().join("event-hub"),
        DurableEventHubConfig::default(),
    )?);
    let control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(config.data_directory()),
        Box::new(DurableEventPublisher::new(Arc::clone(&hub))),
    )?;
    let storage = SqliteStorage::open(config.data_directory())?;
    let outbound = DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(config.data_directory())?,
        WorkerOutboundQueueConfig::default(),
    )?;
    let application = Arc::new(StandaloneControlPlaneApplication::new(
        control_plane,
        storage,
        outbound,
        Arc::clone(&hub),
        ProductSessionExecutionConfig::try_new(
            match scope.clone() {
                Scope::RepositoryScope(scope) => scope,
                _ => return Err("repository scope fixture is invalid".into()),
            },
            "fixture-checkout-revision",
            "codex-chat",
            3_600,
            1_073_741_824,
        )?,
    )?);
    let api = Arc::new(GeneratedContractDispatcher::new(application));
    let sessions = Arc::new(SqliteAuthSessionManager::open(
        config.data_directory().join("auth-sessions"),
        vec![AuthSessionBootstrap::new(
            required_environment("WWC_SERVER_BOOTSTRAP_PROOF")?,
            actor.clone(),
            vec![scope.clone()],
        )?],
        AuthSessionConfig::new(Duration::from_mins(10), Duration::from_hours(8))?,
    )?);
    let authenticator: Arc<dyn RequestAuthenticator> = sessions.clone();
    let principal = AuthenticatedPrincipal::new(actor.clone(), vec![scope.clone()])?;
    let running = start_server(config, Arc::clone(&sessions), authenticator, api, None).await?;
    println!(
        "{{\"status\":\"ready\",\"port\":{}}}",
        running.local_address().port()
    );

    let mut revoke = signal(SignalKind::user_defined1())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        value = revoke.recv() => {
            if value.is_none() {
                return Err("authorization revocation signal stream closed".into());
            }
            hub.revoke_authorization(
                &principal,
                &scope,
                &ControlPlaneWebSocketAuthorizationEpoch(2),
            )?;
            sessions.replace_authorized_scopes(&actor, Vec::new())?;
            tokio::signal::ctrl_c().await?;
        }
    }
    running.shutdown().await?;
    Ok(())
}

fn seed_once(data_directory: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let marker = data_directory.join("browser-local-controls.seeded");
    if marker.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(data_directory)?;
    let mut storage = SqliteStorage::open(data_directory)?;
    seed_provider_and_credential(&mut storage)?;
    let runtime = prepare_runtime(&mut storage)?;
    let session_revision = prepare_product_session(&mut storage, &runtime)?;
    prepare_approval(&mut storage, &runtime, session_revision)?;
    Box::new(storage).close()?;
    std::fs::write(marker, b"v1\n")?;
    Ok(())
}

fn seed_provider_and_credential(storage: &mut SqliteStorage) -> Result<(), Box<dyn Error>> {
    ProviderCatalogService::new(storage).upsert(
        &ProviderCatalogRequest {
            actor: actor(),
            scope: Scope::OrganizationScope(organization_scope()),
            request_id: RequestId(id("req", 100)),
            expected_catalog_version: 0,
        },
        &ProviderDescriptor {
            provider_id: "browser-provider".into(),
            display_name: "Browser Provider".into(),
            adapter_kind: "fixture-adapter".into(),
            credential_reference_id: CredentialReferenceId(id("crd", 1)),
            models: vec![ModelCapability {
                model_id: "browser-model".into(),
                display_name: "Browser Model".into(),
                context_window_tokens: 128_000,
                max_output_tokens: 16_000,
                tool_support: ModelToolSupport::Parallel,
                structured_output_support: StructuredOutputSupport::JsonSchemaStrict,
                reasoning_efforts: vec!["high".into(), "medium".into()],
            }],
        },
    )?;
    CredentialReferenceService::new(storage).create(
        &CredentialReferenceCreateCommand {
            actor: actor(),
            command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
            expected_revision: Revision(0),
            payload: CredentialReferenceCreatePayload {
                credential_reference_id: CredentialReferenceId(id("crd", 1)),
                display_name: "Primary browser Provider credential".into(),
                provider_id: "browser-provider".into(),
                vault_locator: "fixture-vault://primary".into(),
            },
            request_id: RequestId(id("req", 101)),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: repository_scope(),
        },
        1_775_000_000_000,
    )?;
    Ok(())
}

fn prepare_runtime(storage: &mut SqliteStorage) -> Result<WorkerSlotAuthority, Box<dyn Error>> {
    let registration = WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "browser-local-controls".into(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["codex".into()],
        capability_digest: digest('a'),
        security_zone: "local".into(),
        max_slots: 2,
        message_id: ExecutionMessageId(id("xmsg", 1)),
        request_id: RequestId(id("req", 200)),
        sent_at: at(1),
        started_at: at(1),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    };
    let heartbeat = WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: 2,
        heartbeat_sequence: ExecutionSequence(1),
        max_slots: 2,
        running_slots: 0,
        message_id: ExecutionMessageId(id("xmsg", 2)),
        observed_at: at(2),
        sent_at: at(2),
        worker_id: registration.worker_id.clone(),
        worker_instance_id: registration.worker_instance_id.clone(),
    };
    {
        let mut registry = storage.execution_registry()?;
        registry.register_worker(&registration)?;
        let receipt = registry.record_heartbeat(&heartbeat)?;
        if receipt.status != LeaseWriteStatus::Accepted {
            return Err("fixture heartbeat was not accepted".into());
        }
    }
    configure_admission(storage)?;
    let lease = lease();
    let receipt = storage.execution_registry()?.claim_execution_job(&lease)?;
    if receipt.status != LeaseWriteStatus::Accepted {
        return Err("fixture lease was not accepted".into());
    }
    open_slot(storage, &lease)
}

fn configure_admission(storage: &mut SqliteStorage) -> Result<(), Box<dyn Error>> {
    let scope = execution_scope();
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 2,
        max_queued: 2,
        token_budget: 10_000,
        cost_budget_microunits: 10_000,
        max_runtime_millis: 60_000,
    };
    let mut admission = storage.execution_admission()?;
    for boundary in admission_boundaries(&scope) {
        admission.configure_policy(&ExecutionAdmissionPolicy { boundary, limits })?;
    }
    admission.reserve(&ExecutionReservationRequest {
        scope: scope.clone(),
        user_id: UserId(id("usr", 1)),
        worker_pool_id: pool(),
        job_id: ExecutionJobId(id("job", 1)),
        request_id: RequestId(id("req", 210)),
        repository_access: ExecutionRepositoryAccess::ReadOnly,
        reserved_tokens: 100,
        reserved_cost_microunits: 100,
        runtime_limit_millis: 30_000,
        submitted_at: at(3),
    })?;
    admission.start(&ExecutionReservationStart {
        scope,
        worker_pool_id: pool(),
        job_id: ExecutionJobId(id("job", 1)),
        request_id: RequestId(id("req", 211)),
        expected_revision: 1,
        started_at: at(4),
    })?;
    Ok(())
}

fn admission_boundaries(scope: &ExecutionQueueScope) -> Vec<ExecutionAdmissionBoundary> {
    vec![
        ExecutionAdmissionBoundary::Organization {
            organization_id: scope.organization_id.clone(),
        },
        ExecutionAdmissionBoundary::Project {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
        },
        ExecutionAdmissionBoundary::Repository {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: pool(),
        },
    ]
}

fn lease() -> ExecutionLeaseClaim {
    ExecutionLeaseClaim {
        expires_at: expires_at(),
        fencing_token: FencingToken("1".into()),
        issued_at: at(5),
        job_id: ExecutionJobId(id("job", 1)),
        lease_id: LeaseId(id("lse", 1)),
        message_id: ExecutionMessageId(id("xmsg", 12)),
        payload_digest: digest('b'),
        request_id: RequestId(id("req", 212)),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
        attempt: 1,
    }
}

fn open_slot(
    storage: &mut SqliteStorage,
    lease: &ExecutionLeaseClaim,
) -> Result<WorkerSlotAuthority, Box<dyn Error>> {
    let authority = WorkerSlotAuthority {
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_session_id: WorkerSessionId(id("wsn", 1)),
        codex_thread_id: CodexThreadId(id("cdx", 1)),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        attempt: lease.attempt,
        fencing_token: lease.fencing_token.clone(),
    };
    let mut slots = storage.worker_session_slots()?;
    slots.configure_resources(
        &authority.worker_id,
        &authority.worker_instance_id,
        WorkerSlotResourceLimits {
            max_memory_bytes: 1_000,
            max_disk_bytes: 1_000,
            max_processes: 4,
        },
    )?;
    slots.open(&WorkerSlotOpenRequest {
        authority: authority.clone(),
        resources: WorkerSlotResources {
            memory_bytes: 10,
            disk_bytes: 10,
            process_slots: 1,
        },
        request_id: RequestId(id("req", 213)),
        opened_at: at(6),
    })?;
    Ok(authority)
}

fn prepare_product_session(
    storage: &mut SqliteStorage,
    runtime: &WorkerSlotAuthority,
) -> Result<u64, Box<dyn Error>> {
    let scope_key = receipt_scope_key()?;
    let mut service = ProductSessionService::new(storage);
    service.create(&winwincode_control_plane::CreateProductSessionCommand {
        context: product_context(&scope_key, 300, 0, at(7))?,
        product_session_id: ProductSessionId(id("psn", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
        title: "Browser local controls".into(),
        model_route: model_route(),
    })?;
    let binding = SessionBindingIdentity::product_session(
        ProductSessionId(id("psn", 1)),
        runtime.job_id.clone(),
    )?;
    let receipt = service.continue_session(&ContinueProductSessionCommand {
        context: product_context(&scope_key, 301, 1, at(8))?,
        product_session_id: ProductSessionId(id("psn", 1)),
        binding_identity: binding,
        runtime_authority: runtime.clone(),
        execution_scope: execution_scope(),
        worker_pool_id: pool(),
        model_exchange_id: ModelExchangeId(id("mdl", 1)),
    })?;
    Ok(receipt.record.session().revision())
}

fn prepare_approval(
    storage: &mut SqliteStorage,
    runtime: &WorkerSlotAuthority,
    product_session_revision: u64,
) -> Result<(), Box<dyn Error>> {
    let scope_key = receipt_scope_key()?;
    let authority = GateInteractionAuthority {
        execution_scope: execution_scope(),
        worker_pool_id: pool(),
        product_session_revision,
        stage_run_id: None,
        job_revision: 2,
        worker_slot_revision: 1,
        runtime: runtime.clone(),
        lease_expires_at: expires_at(),
        gate: GateDecisionFact {
            decision: RoutableGateDecision::from_gate(&GateDecision::RequestPlanDelta {
                reason: "browser approval fixture".into(),
            })?,
            action_id: "action:shell:browser".into(),
            action_digest: digest('c'),
            envelope_version: 1,
            envelope_digest: digest('d'),
            decision_revision: 1,
            candidate: Some(GateCandidateIdentity {
                candidate_ref: format!("git-candidate:sha256:{}", "e".repeat(64)),
                candidate_digest: digest('e'),
                candidate_revision: 1,
            }),
        },
    };
    GateInteractionService::new(storage).register(&RegisterGateInteractionCommand {
        context: GateInteractionCommandContext {
            receipt_identity: receipt(&scope_key, 400)?,
            event_id: ControlPlaneEventId(id("evt", 400)),
            occurred_at: at(9),
        },
        subject: GateInteractionSubject::Approval(ApprovalId(id("apr", 1))),
        authority: authority.clone(),
        authorized_actor: GateInteractionActor::User(UserId(id("usr", 1))),
        expires_at: expires_at(),
        attention_decisions: Vec::new(),
    })?;
    ChatInteractionService::new(storage).record_approval(&RecordApprovalInteractionCommand {
        public_scope: public_scope(),
        request: approval_request(runtime)?,
    })?;
    Ok(())
}

fn approval_request(
    runtime: &WorkerSlotAuthority,
) -> Result<ApprovalRequestMessage, Box<dyn Error>> {
    Ok(serde_json::from_value(json!({
        "action": {
            "category": "shell",
            "details": {
                "contentType": "application/json",
                "dataBase64": "QlJPV1NFUl9QUklWQVRFX0FDVElPTg==",
                "payloadDigest": digest('f')
            },
            "summary": "Run the browser fixture action."
        },
        "approvalId": id("apr", 1),
        "expiresAt": expires_at(),
        "kind": "approval.request",
        "lease": {
            "attempt": runtime.attempt,
            "expiresAt": expires_at(),
            "fencingToken": runtime.fencing_token,
            "issuedAt": at(5),
            "jobId": runtime.job_id,
            "leaseId": runtime.lease_id,
            "workerId": runtime.worker_id,
            "workerInstanceId": runtime.worker_instance_id
        },
        "messageId": id("xmsg", 410),
        "requestId": id("req", 410),
        "schemaVersion": "winwincode/v1",
        "sentAt": at(10),
        "sessionIdentity": {
            "codexThreadId": runtime.codex_thread_id,
            "productSessionId": id("psn", 1),
            "workerSessionId": runtime.worker_session_id
        },
        "workerSessionId": runtime.worker_session_id
    }))?)
}

fn product_context(
    scope: &ReceiptScopeKey,
    request: u64,
    expected_revision: u64,
    occurred_at: Instant,
) -> Result<ProductSessionCommandContext, Box<dyn Error>> {
    Ok(ProductSessionCommandContext {
        receipt_identity: receipt(scope, request)?,
        expected_revision,
        event_id: ControlPlaneEventId(id("evt", request)),
        occurred_at,
        public_actor: public_actor(),
        public_scope: public_scope(),
    })
}

fn receipt(scope: &ReceiptScopeKey, request: u64) -> Result<ReceiptIdentity, Box<dyn Error>> {
    Ok(ReceiptIdentity::new(
        receipt_actor_key()?,
        scope.clone(),
        RequestId(id("req", request)),
    )?)
}

fn receipt_actor_key() -> Result<ReceiptActorKey, Box<dyn Error>> {
    Ok(winwincode_storage::receipt_actor_key(&public_actor())?)
}

fn receipt_scope_key() -> Result<ReceiptScopeKey, Box<dyn Error>> {
    Ok(winwincode_storage::receipt_scope_key(&public_scope())?)
}

fn actor() -> Actor {
    Actor::UserActor(UserActor {
        kind: UserActorKind::User,
        id: UserId(id("usr", FIXTURE_SEED)),
    })
}

fn organization_scope() -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", FIXTURE_SEED)),
    }
}

fn repository_scope() -> Scope {
    Scope::RepositoryScope(RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", FIXTURE_SEED)),
        workspace_id: WorkspaceId(id("wsp", FIXTURE_SEED)),
        project_id: ProjectId(id("prj", FIXTURE_SEED)),
        repository_id: RepositoryId(id("rep", FIXTURE_SEED)),
    })
}

fn public_actor() -> PublicEventActor {
    PublicEventActor::User {
        id: UserId(id("usr", FIXTURE_SEED)),
    }
}

fn public_scope() -> PublicEventScope {
    PublicEventScope::Repository {
        organization_id: OrganizationId(id("org", FIXTURE_SEED)),
        workspace_id: WorkspaceId(id("wsp", FIXTURE_SEED)),
        project_id: ProjectId(id("prj", FIXTURE_SEED)),
        repository_id: RepositoryId(id("rep", FIXTURE_SEED)),
    }
}

fn execution_scope() -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: OrganizationId(id("org", FIXTURE_SEED)),
        workspace_id: WorkspaceId(id("wsp", FIXTURE_SEED)),
        project_id: ProjectId(id("prj", FIXTURE_SEED)),
        repository_id: RepositoryId(id("rep", FIXTURE_SEED)),
        product_session_id: ProductSessionId(id("psn", FIXTURE_SEED)),
        delivery_id: None,
    }
}

fn pool() -> WorkerPoolId {
    WorkerPoolId(id("wpl", FIXTURE_SEED))
}

fn model_route() -> ModelRoute {
    ModelRoute {
        provider_id: "browser-provider".into(),
        model_id: "browser-model".into(),
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
    }
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2026-08-01T00:00:{second:02}.000Z"))
}

fn expires_at() -> Instant {
    Instant("2030-01-01T00:00:00.000Z".into())
}

fn digest(value: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", value.to_string().repeat(64)))
}

fn environment_config() -> Result<ServerConfig, Box<dyn Error>> {
    let bind_address: SocketAddr = required_environment("WWC_SERVER_BIND")?.parse()?;
    let public_url = required_environment("WWC_SERVER_PUBLIC_URL")?;
    let data_directory = PathBuf::from(required_environment("WWC_SERVER_DATA_DIRECTORY")?);
    let allowed_origins = required_environment("WWC_SERVER_ALLOWED_ORIGINS")?
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let tls = ServerTls::Pem {
        certificate_path: PathBuf::from(required_environment("WWC_SERVER_TLS_CERTIFICATE")?),
        private_key_path: PathBuf::from(required_environment("WWC_SERVER_TLS_PRIVATE_KEY")?),
    };
    Ok(ServerConfig::new(
        bind_address,
        public_url,
        tls,
        allowed_origins,
        data_directory,
        Duration::from_secs(30),
    )?)
}

fn required_environment(name: &str) -> Result<String, Box<dyn Error>> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is required").into())
}
