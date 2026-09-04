// SPDX-License-Identifier: Apache-2.0

#![recursion_limit = "256"]

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, ModelRoute, OrganizationScope, OrganizationScopeKind, Scope,
};
use winwincode_codex::{
    HelperReleaseManifest, ProductionCodexAdapter, ProductionCodexConfig, ProductionCodexOptions,
};
use winwincode_control_plane::{
    CanonicalEnterpriseIdentityLifecycle, CatalogAvailability, CollaborationService, ControlPlane,
    ControlPlaneConfig, ControlPlaneInstanceRuntimeConfig, CredentialReferenceErrorKind,
    CredentialReferenceService, DurableWorkerInteractionOutbound,
    EnterpriseIdentityProductionVerifiers, EnterpriseIdentityProtocolAdapter,
    EnterpriseIdentityProtocolConfig, EnterpriseIdentityService, EnterpriseIdentityVerifierConfig,
    EnterpriseIdentityVerifierTimeouts, EnterpriseRbacService, LocalDeliveryAdapterConfig,
    LocalModelPolicyAuthority, LocalModelPolicyAuthorityConfig, LocalPublicationAdapterConfig,
    LocalSecretStoreAdapter, ModelAdmissionLimits, ModelAdmissionPolicyLayer, ModelCapability,
    ModelRequestPoolConfig, ModelRoutePolicyDecision, ModelSettingsRequest, ModelSettingsService,
    ModelSettingsTarget, ModelSettingsValues, ModelToolSupport, ProductSessionExecutionApplication,
    ProductSessionExecutionConfig, ProviderAdmissionReservationConfig, ProviderCatalogRequest,
    ProviderCatalogService, ProviderDescriptor, ResolvedSecret, SecretStorePort,
    StandaloneModelExecutionApplication, StandaloneModelExecutionConfig, StandaloneProviderConfig,
    StructuredOutputSupport, TrustedProtocolParty, local_loopback_retry_policy,
};
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, ProjectId, RepositoryId, RequestId, Revision,
    Sha256Digest, UserId, WorkerId, WorkerInstanceId, WorkspaceId,
};
use winwincode_domain::{
    RepositoryScope, RepositoryScopeKind, SchemaVersion, UserActor, UserActorKind,
};
use winwincode_execution_port::{
    action_enforcement::{ActionEnforcementIssuer, ActionEnforcementSigningKey},
    action_gateway::ExecutionEnvelopeToken,
    generated::{
        ExecutionPortMessage, ModelGatewayRoute, WorkerCapabilityFeature, WorkerCapabilitySet,
        WorkerCapabilitySetPlatform,
    },
    transport::ExecutionPortCore,
};
use winwincode_local::LocalLauncherConfig;
use winwincode_server::{
    AuthSessionBootstrap, AuthSessionConfig, DurableEventHub, DurableEventHubConfig,
    DurableEventPublisher, EnterpriseIdentityManagementApplication,
    EnterpriseIdentityProtocolApplication, EnterpriseRbacManagementApplication,
    EnterpriseRequestAuthenticator, FileRemoteWorkerAuthenticator, GeneratedContractDispatcher,
    LocalRuntimeSupervisor, ProductionRemoteWorkerExchange, RemoteWorkerExchangePort,
    RepositoryRuntimeScheduler, RequestAuthenticator, ServerConfig, ServerExecutionPortCore,
    ServerTls, SqliteAuthSessionManager, StandaloneApplicationClock,
    StandaloneControlPlaneApplication, SystemStandaloneApplicationClock,
    UnavailableEnterpriseManagementApplication, start_server, start_server_with_remote_worker,
};
use winwincode_storage::{
    ProductStateStorage, SqliteStorage, WorkerOutboundQueueConfig, WorkerPoolId,
    WorkerRegistryScope,
};
use winwincode_worker::WorkerConfig;

const SERVER_TOKIO_WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("winwincode-server")
        .thread_stack_size(SERVER_TOKIO_WORKER_STACK_BYTES)
        .build()
        .unwrap_or_else(|error| panic!("failed to create WinWinCode Server runtime: {error}"));
    if let Err(error) = runtime.block_on(run()) {
        eprintln!("winwincode-server: {error}");
        std::process::exit(1);
    }
}

type ProductionExecutionPort = ServerExecutionPortCore<
    ProductSessionExecutionApplication<StandaloneModelExecutionApplication>,
>;
type ProductionSupervisor = LocalRuntimeSupervisor<ProductionExecutionPort, ProductionCodexAdapter>;

struct ProductionStartup {
    config: ServerConfig,
    delivery: LocalDeliveryAdapterConfig,
    publication: LocalPublicationAdapterConfig,
    repository_scope: RepositoryScope,
    source_root: PathBuf,
    execution_config: ProductSessionExecutionConfig,
    subject: String,
    model_route: LocalModelRoute,
    auth_bootstrap: AuthSessionBootstrap,
    auth_config: AuthSessionConfig,
}

struct ProductionApplicationComposition {
    config: ServerConfig,
    repository_scope: RepositoryScope,
    source_root: PathBuf,
    subject: String,
    model_route: LocalModelRoute,
    auth_bootstrap: AuthSessionBootstrap,
    auth_config: AuthSessionConfig,
    application: StandaloneControlPlaneApplication,
    identities: Arc<EnterpriseIdentityService>,
    rbac: Arc<EnterpriseRbacService>,
}

struct ComposedApplication {
    application: StandaloneControlPlaneApplication,
    identities: Arc<EnterpriseIdentityService>,
    rbac: Arc<EnterpriseRbacService>,
}

fn load_production_startup() -> Result<ProductionStartup, Box<dyn std::error::Error>> {
    let config = environment_config()?;
    let (delivery, publication, repository_scope) = local_production_configs()?;
    let source_root = match env::var_os("WWC_SERVER_SOURCE_ROOT") {
        Some(value) => PathBuf::from(value),
        None => delivery
            .repository_root()
            .parent()
            .map(PathBuf::from)
            .ok_or("configured Delivery repository has no controlled source root")?,
    };
    let execution_config = ProductSessionExecutionConfig::try_new(
        repository_scope.clone(),
        required_environment("WWC_SERVER_CHECKOUT_REVISION")?,
        required_environment_or("WWC_SERVER_EXECUTION_PROFILE", "codex-chat")?,
        optional_i64("WWC_SERVER_MAX_RUNTIME_SECONDS", 3_600)?,
        optional_i64("WWC_SERVER_MAX_ARTIFACT_BYTES", 1_073_741_824)?,
    )?;
    let bootstrap_proof = required_environment("WWC_SERVER_BOOTSTRAP_PROOF")?;
    let subject = required_environment("WWC_SERVER_AUTH_SUBJECT")?;
    let model_route = LocalModelRoute::from_environment()?;
    let auth_bootstrap = AuthSessionBootstrap::new(
        bootstrap_proof,
        Actor::UserActor(UserActor {
            kind: UserActorKind::User,
            id: UserId(subject.clone()),
        }),
        vec![Scope::RepositoryScope(repository_scope.clone())],
    )?;
    let auth_config = AuthSessionConfig::new(
        optional_duration_seconds("WWC_SERVER_BOOTSTRAP_WINDOW_SECONDS", 10 * 60)?,
        optional_duration_seconds("WWC_SERVER_SESSION_TTL_SECONDS", 8 * 60 * 60)?,
    )?;
    Ok(ProductionStartup {
        config,
        delivery,
        publication,
        repository_scope,
        source_root,
        execution_config,
        subject,
        model_route,
        auth_bootstrap,
        auth_config,
    })
}

fn open_production_application(
    startup: ProductionStartup,
) -> Result<ProductionApplicationComposition, Box<dyn std::error::Error>> {
    let ProductionStartup {
        config,
        delivery,
        publication,
        repository_scope,
        source_root,
        execution_config,
        subject,
        model_route,
        auth_bootstrap,
        auth_config,
    } = startup;
    let hub = Arc::new(DurableEventHub::open(
        config.data_directory().join("event-hub"),
        DurableEventHubConfig::default(),
    )?);
    let control_plane = match ControlPlane::start_local_with_production_adapters(
        ControlPlaneConfig::local(config.data_directory()),
        Box::new(DurableEventPublisher::new(Arc::clone(&hub))),
        delivery,
        publication,
    ) {
        Ok(control_plane) => control_plane,
        Err(error) => {
            let _ = hub.close();
            return Err(Box::new(error));
        }
    };
    let mut storage = match SqliteStorage::open(config.data_directory()) {
        Ok(storage) => storage,
        Err(error) => {
            let _ = control_plane.shutdown();
            let _ = hub.close();
            return Err(Box::new(error));
        }
    };
    configure_local_model_authority(
        &mut storage,
        &subject,
        &repository_scope,
        &model_route,
        PathBuf::from(required_environment("SECRET_DIRECTORY")?),
    )?;
    let worker_outbound_storage = match SqliteStorage::open(config.data_directory()) {
        Ok(storage) => storage,
        Err(error) => {
            let _ = control_plane.shutdown();
            let _ = Box::new(storage).close();
            let _ = hub.close();
            return Err(Box::new(error));
        }
    };
    let worker_outbound = match DurableWorkerInteractionOutbound::new(
        worker_outbound_storage,
        WorkerOutboundQueueConfig::default(),
    ) {
        Ok(worker_outbound) => worker_outbound,
        Err(error) => {
            let _ = control_plane.shutdown();
            let _ = Box::new(storage).close();
            let _ = hub.close();
            return Err(Box::new(error));
        }
    };
    let ComposedApplication {
        application,
        identities,
        rbac,
    } = compose_production_application(
        &config,
        control_plane,
        storage,
        worker_outbound,
        hub,
        execution_config,
    )?;
    Ok(ProductionApplicationComposition {
        config,
        repository_scope,
        source_root,
        subject,
        model_route,
        auth_bootstrap,
        auth_config,
        application,
        identities,
        rbac,
    })
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let startup = load_production_startup()?;
    let composition = open_production_application(startup)?;
    Box::pin(run_composed_server(composition)).await
}

async fn run_composed_server(
    composition: ProductionApplicationComposition,
) -> Result<(), Box<dyn std::error::Error>> {
    let ProductionApplicationComposition {
        config,
        repository_scope,
        source_root,
        subject,
        model_route,
        auth_bootstrap,
        auth_config,
        application,
        identities,
        rbac,
    } = composition;
    let model_execution = open_local_model_execution(&config, &model_route)?;
    let action_signing_key = configured_action_signing_key()?;
    let delegate = ProductSessionExecutionApplication::new_with_action_issuer(
        model_execution,
        ActionEnforcementIssuer::new(action_signing_key.clone()),
    );
    let execution_port =
        ServerExecutionPortCore::from_application(&application, repository_scope.clone(), delegate);
    let clock: Arc<dyn StandaloneApplicationClock> = Arc::new(SystemStandaloneApplicationClock);
    let worker_id = WorkerId(required_environment_or(
        "WWC_SERVER_WORKER_ID",
        "wrk_00000000000000000000000001",
    )?);
    // A process restart must enter the scheduler as a new Worker instance and
    // generation. Stable defaults would make a restarted process look like
    // the predecessor and would suppress the repository replacement path.
    let worker_instance_id =
        WorkerInstanceId(runtime_identity("WWC_SERVER_WORKER_INSTANCE_ID", "wki_")?);
    let scheduler_generation = runtime_identity("WWC_SERVER_SCHEDULER_GENERATION", "gen_")?;
    let capabilities = worker_capabilities()?;
    let worker_config = WorkerConfig {
        worker_id: worker_id.clone(),
        worker_instance_id: worker_instance_id.clone(),
        started_at: clock.now_instant(),
        capabilities: capabilities.clone(),
    };
    let launcher_config = LocalLauncherConfig::try_new(
        config.data_directory(),
        source_root,
        clock.now_millis(),
        ControlPlaneInstanceRuntimeConfig::default(),
        256,
    )?;
    let worker_pool_id = WorkerPoolId(required_environment_or(
        "WWC_SERVER_WORKER_POOL_ID",
        "wpl_00000000000000000000000001",
    )?);
    let scheduler = RepositoryRuntimeScheduler::from_application(
        &application,
        repository_scope.clone(),
        worker_id.clone(),
        worker_instance_id,
        scheduler_generation,
        Duration::from_secs(30),
    )?
    .with_admission_identity(UserId(subject.clone()), worker_pool_id)?;
    let worker_mode = required_environment_or("WWC_SERVER_WORKER_MODE", "local")?;
    if worker_mode != "local" && worker_mode != "remote" {
        return Err("WWC_SERVER_WORKER_MODE must be local or remote".into());
    }
    if worker_mode == "remote" {
        return Box::pin(run_remote_composition(RemoteRuntimeComposition {
            config,
            repository_scope,
            subject,
            auth_bootstrap,
            auth_config,
            application,
            identities,
            rbac,
            worker_id,
            scheduler,
            execution_port,
            clock,
        }))
        .await;
    }
    Box::pin(run_local_composition(LocalRuntimeComposition {
        config,
        repository_scope,
        subject,
        model_route,
        auth_bootstrap,
        auth_config,
        application,
        identities,
        rbac,
        capabilities,
        action_signing_key,
        launcher_config,
        worker_config,
        execution_port,
        scheduler,
        clock,
    }))
    .await
}

struct LocalRuntimeComposition {
    config: ServerConfig,
    repository_scope: RepositoryScope,
    subject: String,
    model_route: LocalModelRoute,
    auth_bootstrap: AuthSessionBootstrap,
    auth_config: AuthSessionConfig,
    application: StandaloneControlPlaneApplication,
    identities: Arc<EnterpriseIdentityService>,
    rbac: Arc<EnterpriseRbacService>,
    capabilities: WorkerCapabilitySet,
    action_signing_key: ActionEnforcementSigningKey,
    launcher_config: LocalLauncherConfig,
    worker_config: WorkerConfig,
    execution_port: ProductionExecutionPort,
    scheduler: RepositoryRuntimeScheduler,
    clock: Arc<dyn StandaloneApplicationClock>,
}

async fn run_local_composition(
    composition: LocalRuntimeComposition,
) -> Result<(), Box<dyn std::error::Error>> {
    let LocalRuntimeComposition {
        config,
        repository_scope,
        subject,
        model_route,
        auth_bootstrap,
        auth_config,
        application,
        identities,
        rbac,
        capabilities,
        action_signing_key,
        launcher_config,
        worker_config,
        execution_port,
        scheduler,
        clock,
    } = composition;
    let codex = open_production_codex(&config, &model_route, capabilities, action_signing_key)?;
    let supervisor = Box::pin(LocalRuntimeSupervisor::start_with_scheduler(
        launcher_config,
        worker_config,
        execution_port,
        codex,
        Arc::clone(&clock),
        Duration::from_millis(25),
        Some(Box::new(scheduler)),
    ))
    .await?;
    let application =
        Arc::new(application.with_runtime_health(Arc::new(supervisor.health_handle())));
    let api = Arc::new(GeneratedContractDispatcher::new(application));
    let auth_sessions = Arc::new(SqliteAuthSessionManager::open(
        config.data_directory().join("auth-sessions"),
        vec![auth_bootstrap],
        auth_config,
    )?);
    let authenticator: Arc<dyn RequestAuthenticator> = Arc::new(
        EnterpriseRequestAuthenticator::new(Arc::clone(&auth_sessions), Arc::clone(&identities)),
    );
    let enterprise_identity = compose_enterprise_identity_protocol(
        &config,
        &subject,
        &repository_scope.organization_id,
        Arc::clone(&auth_sessions),
        identities,
        rbac,
    )?;
    serve_runtime(
        config,
        auth_sessions,
        authenticator,
        api,
        enterprise_identity,
        supervisor,
    )
    .await
}

struct RemoteRuntimeComposition<Core> {
    config: ServerConfig,
    repository_scope: RepositoryScope,
    subject: String,
    auth_bootstrap: AuthSessionBootstrap,
    auth_config: AuthSessionConfig,
    application: StandaloneControlPlaneApplication,
    identities: Arc<EnterpriseIdentityService>,
    rbac: Arc<EnterpriseRbacService>,
    worker_id: WorkerId,
    scheduler: RepositoryRuntimeScheduler,
    execution_port: Core,
    clock: Arc<dyn StandaloneApplicationClock>,
}

async fn run_remote_composition<Core>(
    composition: RemoteRuntimeComposition<Core>,
) -> Result<(), Box<dyn std::error::Error>>
where
    Core: ExecutionPortCore<Output = Vec<ExecutionPortMessage>> + Send + 'static,
    Core::Error: Send + std::fmt::Display,
{
    let RemoteRuntimeComposition {
        config,
        repository_scope,
        subject,
        auth_bootstrap,
        auth_config,
        application,
        identities,
        rbac,
        worker_id,
        scheduler,
        execution_port,
        clock,
    } = composition;
    let remote_worker_scope = WorkerRegistryScope::Repository {
        organization_id: repository_scope.organization_id.clone(),
        workspace_id: repository_scope.workspace_id.clone(),
        project_id: repository_scope.project_id.clone(),
        repository_id: repository_scope.repository_id.clone(),
    };
    let remote_authenticator = Arc::new(FileRemoteWorkerAuthenticator::open(
        PathBuf::from(required_environment(
            "WWC_SERVER_REMOTE_WORKER_CREDENTIAL_FILE",
        )?),
        worker_id,
        WorkerPoolId(required_environment_or(
            "WWC_SERVER_WORKER_POOL_ID",
            "wpl_00000000000000000000000001",
        )?),
        remote_worker_scope,
        required_environment_or("WWC_SERVER_REMOTE_WORKER_ISSUER", "winwincode-server")?,
        required_environment_or("WWC_SERVER_REMOTE_WORKER_SUBJECT", "remote-worker")?,
        required_environment_or("WWC_SERVER_REMOTE_WORKER_SECURITY_ZONE", "default")?,
        winwincode_domain::Instant(required_environment("WWC_SERVER_REMOTE_WORKER_EXPIRES_AT")?),
        &clock.now_instant(),
    )?);
    let exchange: Arc<dyn RemoteWorkerExchangePort> =
        Arc::new(ProductionRemoteWorkerExchange::new(
            config.data_directory(),
            remote_authenticator,
            scheduler,
            execution_port,
        ));
    let api = Arc::new(GeneratedContractDispatcher::new(Arc::new(application)));
    let auth_sessions = Arc::new(SqliteAuthSessionManager::open(
        config.data_directory().join("auth-sessions"),
        vec![auth_bootstrap],
        auth_config,
    )?);
    let authenticator: Arc<dyn RequestAuthenticator> = Arc::new(
        EnterpriseRequestAuthenticator::new(Arc::clone(&auth_sessions), Arc::clone(&identities)),
    );
    let enterprise_identity = compose_enterprise_identity_protocol(
        &config,
        &subject,
        &repository_scope.organization_id,
        Arc::clone(&auth_sessions),
        identities,
        rbac,
    )?;
    serve_remote_runtime(
        config,
        auth_sessions,
        authenticator,
        api,
        enterprise_identity,
        exchange,
    )
    .await
}

async fn serve_remote_runtime(
    config: ServerConfig,
    auth_sessions: Arc<SqliteAuthSessionManager>,
    authenticator: Arc<dyn RequestAuthenticator>,
    api: Arc<GeneratedContractDispatcher>,
    enterprise_identity: Option<Arc<EnterpriseIdentityProtocolApplication>>,
    exchange: Arc<dyn RemoteWorkerExchangePort>,
) -> Result<(), Box<dyn std::error::Error>> {
    let running = start_server_with_remote_worker(
        config,
        auth_sessions,
        authenticator,
        api,
        enterprise_identity,
        Some(exchange),
    )
    .await?;
    tokio::signal::ctrl_c().await?;
    running.shutdown().await?;
    Ok(())
}

async fn serve_runtime(
    config: ServerConfig,
    auth_sessions: Arc<SqliteAuthSessionManager>,
    authenticator: Arc<dyn RequestAuthenticator>,
    api: Arc<GeneratedContractDispatcher>,
    enterprise_identity: Option<Arc<EnterpriseIdentityProtocolApplication>>,
    supervisor: ProductionSupervisor,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut running = match start_server(
        config,
        auth_sessions,
        authenticator,
        api,
        enterprise_identity,
    )
    .await
    {
        Ok(running) => running,
        Err(error) => {
            let _ = Box::pin(supervisor.shutdown()).await;
            return Err(Box::new(error));
        }
    };
    if let Err(error) = tokio::signal::ctrl_c().await {
        let _ = running.shutdown_listener().await;
        let _ = Box::pin(supervisor.shutdown()).await;
        let _ = running.shutdown_application();
        return Err(Box::new(error));
    }
    let server_result = running.shutdown_listener().await;
    let runtime_result = Box::pin(supervisor.shutdown()).await;
    let application_result = running.shutdown_application();
    server_result?;
    runtime_result?;
    application_result?;
    Ok(())
}

#[derive(Clone, Debug)]
struct LocalModelRoute {
    provider: String,
    model: String,
    credential_reference: CredentialReferenceId,
}

impl LocalModelRoute {
    fn from_environment() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            provider: required_environment_or(
                "WWC_SERVER_MODEL_PROVIDER_ID",
                "winwincode-loopback",
            )?,
            model: required_environment_or("WWC_SERVER_MODEL_ID", "loopback-model")?,
            credential_reference: CredentialReferenceId(required_environment_or(
                "WWC_SERVER_MODEL_CREDENTIAL_REFERENCE_ID",
                "crd_00000000000000000000000001",
            )?),
        })
    }

    fn as_api_route(&self) -> ModelRoute {
        ModelRoute {
            provider_id: self.provider.clone(),
            model_id: self.model.clone(),
            credential_reference_id: self.credential_reference.clone(),
        }
    }
}

fn local_provider_descriptor(model_route: &LocalModelRoute) -> ProviderDescriptor {
    ProviderDescriptor {
        provider_id: model_route.provider.clone(),
        display_name: "WinWinCode local loopback Provider".to_owned(),
        adapter_kind: "deterministic-loopback".to_owned(),
        credential_reference_id: model_route.credential_reference.clone(),
        models: vec![ModelCapability {
            model_id: model_route.model.clone(),
            display_name: "WinWinCode local loopback model".to_owned(),
            context_window_tokens: 128_000,
            max_output_tokens: 16_000,
            tool_support: ModelToolSupport::Parallel,
            structured_output_support: StructuredOutputSupport::JsonSchemaStrict,
            reasoning_efforts: vec!["high".to_owned(), "medium".to_owned()],
        }],
    }
}

fn configure_local_model_authority(
    storage: &mut SqliteStorage,
    subject: &str,
    repository_scope: &RepositoryScope,
    model_route: &LocalModelRoute,
    secret_directory: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let actor = Actor::UserActor(UserActor {
        kind: UserActorKind::User,
        id: UserId(subject.to_owned()),
    });
    let organization_scope = OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: repository_scope.organization_id.clone(),
    };
    let organization = Scope::OrganizationScope(organization_scope.clone());
    let credential_command = CredentialReferenceCreateCommand {
        actor: actor.clone(),
        command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
        expected_revision: Revision(0),
        payload: CredentialReferenceCreatePayload {
            credential_reference_id: model_route.credential_reference.clone(),
            display_name: "WinWinCode local model credential".to_owned(),
            provider_id: model_route.provider.clone(),
            vault_locator: "local-production://loopback".to_owned(),
        },
        request_id: RequestId("req_00000000000000000000000090".to_owned()),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: organization.clone(),
    };
    match CredentialReferenceService::new(storage).create(&credential_command, now_millis()) {
        Ok(_) => {}
        Err(error) if error.kind() == CredentialReferenceErrorKind::WrongState => {
            let current = CredentialReferenceService::new(storage)
                .resolve(&organization, &model_route.credential_reference)?;
            if current.provider_id() != model_route.provider {
                return Err(Box::new(error));
            }
        }
        Err(error) => return Err(Box::new(error)),
    }
    let credential = CredentialReferenceService::new(storage)
        .resolve(&organization, &model_route.credential_reference)?;
    let secret_store = LocalSecretStoreAdapter::open(secret_directory)?;
    secret_store.store(
        &credential,
        ResolvedSecret::from_bytes(b"winwincode-local-loopback-secret".to_vec())?,
    )?;

    let descriptor = local_provider_descriptor(model_route);
    let catalog = ProviderCatalogService::new(storage).project(&organization)?;
    let provider_matches = catalog.providers.iter().any(|provider| {
        provider.provider_id == descriptor.provider_id
            && provider.availability == CatalogAvailability::Enabled
            && provider.credential_reference_id == descriptor.credential_reference_id
            && provider.adapter_kind == descriptor.adapter_kind
            && provider.models.len() == 1
            && provider.models.iter().any(|model| {
                model.model_id == model_route.model
                    && model.availability == CatalogAvailability::Enabled
            })
    });
    if !provider_matches {
        ProviderCatalogService::new(storage).upsert(
            &ProviderCatalogRequest {
                actor: actor.clone(),
                scope: organization.clone(),
                request_id: RequestId("req_00000000000000000000000091".to_owned()),
                expected_catalog_version: catalog.catalog_version,
            },
            &descriptor,
        )?;
    }

    let target = ModelSettingsTarget::Organization {
        scope: organization_scope,
    };
    let route = model_route.as_api_route();
    let settings = ModelSettingsService::new(storage).project(&target)?;
    if settings.default_model_route.as_ref() != Some(&route)
        || settings.worker_concurrency_limit != 1
    {
        ModelSettingsService::new(storage).update(
            &ModelSettingsRequest {
                actor,
                target,
                request_id: RequestId("req_00000000000000000000000092".to_owned()),
                expected_revision: settings.revision,
            },
            ModelSettingsValues {
                default_model_route: Some(route),
                worker_concurrency_limit: 1,
            },
        )?;
    }
    Ok(())
}

fn open_local_model_execution(
    config: &ServerConfig,
    model_route: &LocalModelRoute,
) -> Result<StandaloneModelExecutionApplication, Box<dyn std::error::Error>> {
    let policy = LocalModelPolicyAuthority::try_new(LocalModelPolicyAuthorityConfig {
        base: ModelAdmissionPolicyLayer::try_new(
            "winwincode.server.local-policy.v1".to_owned(),
            1,
            "winwincode.server.local-budget.v1".to_owned(),
            ModelRoutePolicyDecision::Allow,
            ModelAdmissionLimits {
                requests_per_minute: 1_000,
                tokens_per_minute: 1_000_000,
                concurrent_requests: 16,
                token_budget: 10_000_000,
                cost_budget_micros: 10_000_000,
            },
        )?,
        enterprise_ceilings: Vec::new(),
    })?;
    let retry_policy = local_loopback_retry_policy()?;
    let pool = ModelRequestPoolConfig {
        max_routes: 4,
        max_active_per_route: 1,
        max_waiting_per_route: 4,
        max_exchange_records_per_route: 8,
        max_buffered_frames_per_stream: 32,
        max_buffered_bytes_per_stream: 64 * 1024,
        resume_buffered_frames_per_stream: 8,
        resume_buffered_bytes_per_stream: 16 * 1024,
    };
    Ok(StandaloneModelExecutionApplication::open(
        StandaloneModelExecutionConfig {
            data_directory: config.data_directory().to_path_buf(),
            secret_directory: PathBuf::from(required_environment("SECRET_DIRECTORY")?),
            providers: vec![StandaloneProviderConfig::Loopback {
                provider_id: model_route.provider.clone(),
            }],
            admission: ProviderAdmissionReservationConfig::try_new(100, 10)?,
            pool,
            policy: Box::new(policy),
            retry_policy: Box::new(retry_policy),
        },
    )?)
}

fn open_production_codex(
    config: &ServerConfig,
    model_route: &LocalModelRoute,
    capabilities: WorkerCapabilitySet,
    action_signing_key: ActionEnforcementSigningKey,
) -> Result<ProductionCodexAdapter, Box<dyn std::error::Error>> {
    let execution_envelope = ExecutionEnvelopeToken {
        version: 1,
        digest: Sha256Digest(required_environment_or(
            "WWC_SERVER_EXECUTION_ENVELOPE_DIGEST",
            &format!("sha256:{}", "a".repeat(64)),
        )?),
    };
    let helper_release_manifest_path =
        PathBuf::from(required_environment("WWC_SERVER_HELPER_RELEASE_MANIFEST")?);
    let codex_config = ProductionCodexConfig::try_new(ProductionCodexOptions {
        data_directory: config.data_directory().join("worker-runtime"),
        helper_executable: PathBuf::from(required_environment("WWC_SERVER_HELPER_EXECUTABLE")?),
        helper_release_manifest: HelperReleaseManifest::from_file(&helper_release_manifest_path)?,
        provider: model_route.provider.clone(),
        model: model_route.model.clone(),
        gateway_route: ModelGatewayRoute {
            capability: "reasoning".to_owned(),
            route: "embedded-canonical-loopback".to_owned(),
        },
        registered_capabilities: capabilities,
        discovered_capabilities: Vec::new(),
        action_signing_key,
        execution_envelope,
        execution_mode: winwincode_codex::ExecutionMode::from_config(&required_environment_or(
            "WWC_SERVER_EXECUTION_MODE",
            "react",
        )?)
        .ok_or("WWC_SERVER_EXECUTION_MODE contains an unsupported execution mode")?,
        observer_mode: winwincode_codex::ObserverMode::from_config(&required_environment_or(
            "WWC_SERVER_OBSERVER_MODE",
            "off",
        )?)
        .ok_or("WWC_SERVER_OBSERVER_MODE contains an unsupported observer mode")?,
    })?;
    Ok(ProductionCodexAdapter::open(codex_config)?)
}

fn configured_action_signing_key() -> Result<ActionEnforcementSigningKey, Box<dyn std::error::Error>>
{
    Ok(ActionEnforcementSigningKey::from_bytes(parse_hex_key(
        &required_environment_or("WWC_SERVER_ACTION_SIGNING_KEY_HEX", &"1f".repeat(32))?,
    )?)?)
}

fn worker_capabilities() -> Result<WorkerCapabilitySet, Box<dyn std::error::Error>> {
    let platform = match (env::consts::ARCH, env::consts::OS) {
        ("aarch64", "macos") => WorkerCapabilitySetPlatform::Aarch64AppleDarwin,
        ("x86_64", "macos") => WorkerCapabilitySetPlatform::X8664AppleDarwin,
        ("aarch64", "linux") => WorkerCapabilitySetPlatform::Aarch64UnknownLinuxGnu,
        ("x86_64", "linux") => WorkerCapabilitySetPlatform::X8664UnknownLinuxGnu,
        _ => return Err("unsupported local Worker platform".into()),
    };
    Ok(WorkerCapabilitySet {
        capability_digest: Sha256Digest(format!("sha256:{}", "0".repeat(64))),
        features: vec![
            WorkerCapabilityFeature::ArtifactStream,
            WorkerCapabilityFeature::Approval,
            WorkerCapabilityFeature::Git,
            WorkerCapabilityFeature::InteractiveInput,
            WorkerCapabilityFeature::Mcp,
            WorkerCapabilityFeature::ModelProxy,
            WorkerCapabilityFeature::Sandbox,
            WorkerCapabilityFeature::Shell,
        ],
        max_concurrent_jobs: 1,
        platform,
    })
}

fn parse_hex_key(value: &str) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    if value.len() != 64 {
        return Err("WWC_SERVER_ACTION_SIGNING_KEY_HEX must contain 32 bytes".into());
    }
    let mut result = [0_u8; 32];
    for (index, slot) in result.iter_mut().enumerate() {
        let start = index * 2;
        *slot = u8::from_str_radix(&value[start..start + 2], 16)
            .map_err(|_| "WWC_SERVER_ACTION_SIGNING_KEY_HEX is not hexadecimal")?;
    }
    Ok(result)
}

fn runtime_identity(
    environment_name: &str,
    prefix: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    if let Some(value) = env::var_os(environment_name) {
        return value
            .into_string()
            .map_err(|_| format!("{environment_name} is not valid UTF-8").into());
    }
    let mut random = [0_u8; 13];
    getrandom::fill(&mut random)?;
    let mut identity = String::with_capacity(prefix.len() + 26);
    identity.push_str(prefix);
    for byte in random {
        identity.push(char::from(ALPHABET[usize::from(byte >> 4)]));
        identity.push(char::from(ALPHABET[usize::from(byte & 0x0f)]));
    }
    Ok(identity)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn compose_production_application(
    config: &ServerConfig,
    control_plane: ControlPlane,
    storage: SqliteStorage,
    worker_outbound: DurableWorkerInteractionOutbound,
    hub: Arc<DurableEventHub>,
    execution_config: ProductSessionExecutionConfig,
) -> Result<ComposedApplication, Box<dyn std::error::Error>> {
    let identities = Arc::new(EnterpriseIdentityService::new(Box::new(
        SqliteStorage::open(config.data_directory())?,
    )));
    let rbac = Arc::new(EnterpriseRbacService::new(Box::new(SqliteStorage::open(
        config.data_directory(),
    )?)));
    let collaboration = Arc::new(CollaborationService::new(
        SqliteStorage::open(config.data_directory())?,
        Arc::clone(&rbac),
    ));
    let rbac_application = Arc::new(EnterpriseRbacManagementApplication::new(
        Arc::clone(&rbac),
        Arc::new(UnavailableEnterpriseManagementApplication),
    ));
    let enterprise = Arc::new(EnterpriseIdentityManagementApplication::new(
        Arc::clone(&identities),
        rbac_application,
    ));
    let application = StandaloneControlPlaneApplication::new_with_enterprise_and_collaboration(
        control_plane,
        storage,
        worker_outbound,
        hub,
        enterprise,
        collaboration,
        execution_config,
    )?;
    Ok(ComposedApplication {
        application,
        identities,
        rbac,
    })
}

fn compose_enterprise_identity_protocol(
    config: &ServerConfig,
    management_subject: &str,
    organization_id: &OrganizationId,
    auth_sessions: Arc<SqliteAuthSessionManager>,
    identities: Arc<EnterpriseIdentityService>,
    rbac: Arc<EnterpriseRbacService>,
) -> Result<Option<Arc<EnterpriseIdentityProtocolApplication>>, Box<dyn std::error::Error>> {
    if !enterprise_identity_mode_enabled()? {
        return Ok(None);
    }
    let tls_root = fs::read(required_environment(
        "WWC_SERVER_IDENTITY_VERIFIER_TLS_ROOT_DER_FILE",
    )?)?;
    let verifier_config = EnterpriseIdentityVerifierConfig::try_new(
        required_environment("WWC_SERVER_IDENTITY_VERIFIER_ENDPOINT")?,
        EnterpriseIdentityVerifierTimeouts {
            connect: Duration::from_secs(5),
            response: Duration::from_secs(10),
            total: Duration::from_secs(30),
        },
        64 * 1024,
    )?
    .with_specific_tls_roots(vec![tls_root])?;
    let scope = Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: organization_id.clone(),
    });
    let secret_store: Arc<dyn SecretStorePort> = Arc::new(LocalSecretStoreAdapter::open(
        required_environment("SECRET_DIRECTORY")?,
    )?);
    let verifiers = EnterpriseIdentityProductionVerifiers::try_new(
        verifier_config,
        Box::new(SqliteStorage::open(config.data_directory())?),
        secret_store,
        scope,
        CredentialReferenceId(required_environment(
            "WWC_SERVER_IDENTITY_VERIFIER_CREDENTIAL_REFERENCE_ID",
        )?),
    )?;
    let (oidc, saml, scim) = verifiers.into_verifiers();
    let management_actor = Actor::UserActor(UserActor {
        kind: UserActorKind::User,
        id: UserId(management_subject.to_owned()),
    });
    let lifecycle = CanonicalEnterpriseIdentityLifecycle::new(
        identities,
        rbac,
        auth_sessions.clone(),
        management_actor.clone(),
    );
    let protocols = EnterpriseIdentityProtocolAdapter::new(
        Box::new(SqliteStorage::open(config.data_directory())?),
        Box::new(lifecycle),
        Box::new(oidc),
        Box::new(saml),
        Box::new(scim),
        EnterpriseIdentityProtocolConfig {
            organization_id: organization_id.clone(),
            management_actor,
            oidc: TrustedProtocolParty {
                issuer: required_environment("WWC_SERVER_OIDC_ISSUER")?,
                audience: required_environment("WWC_SERVER_OIDC_AUDIENCE")?,
            },
            saml: TrustedProtocolParty {
                issuer: required_environment("WWC_SERVER_SAML_ISSUER")?,
                audience: required_environment("WWC_SERVER_SAML_AUDIENCE")?,
            },
            scim: TrustedProtocolParty {
                issuer: required_environment("WWC_SERVER_SCIM_ISSUER")?,
                audience: required_environment("WWC_SERVER_SCIM_AUDIENCE")?,
            },
            max_clock_skew_millis: required_environment(
                "WWC_SERVER_IDENTITY_MAX_CLOCK_SKEW_MILLIS",
            )?
            .parse()?,
            max_assertion_age_millis: required_environment(
                "WWC_SERVER_IDENTITY_MAX_ASSERTION_AGE_MILLIS",
            )?
            .parse()?,
        },
    )?;
    Ok(Some(Arc::new(EnterpriseIdentityProtocolApplication::new(
        Arc::new(protocols),
        auth_sessions,
    ))))
}

fn enterprise_identity_mode_enabled() -> Result<bool, Box<dyn std::error::Error>> {
    const MODE: &str = "WWC_SERVER_ENTERPRISE_IDENTITY_MODE";
    const CONFIGURATION_NAMES: &[&str] = &[
        "WWC_SERVER_IDENTITY_VERIFIER_ENDPOINT",
        "WWC_SERVER_IDENTITY_VERIFIER_TLS_ROOT_DER_FILE",
        "WWC_SERVER_IDENTITY_VERIFIER_CREDENTIAL_REFERENCE_ID",
        "WWC_SERVER_OIDC_ISSUER",
        "WWC_SERVER_OIDC_AUDIENCE",
        "WWC_SERVER_SAML_ISSUER",
        "WWC_SERVER_SAML_AUDIENCE",
        "WWC_SERVER_SCIM_ISSUER",
        "WWC_SERVER_SCIM_AUDIENCE",
        "WWC_SERVER_IDENTITY_MAX_CLOCK_SKEW_MILLIS",
        "WWC_SERVER_IDENTITY_MAX_ASSERTION_AGE_MILLIS",
    ];
    let mode = match env::var(MODE) {
        Ok(mode) => mode,
        Err(env::VarError::NotPresent)
            if CONFIGURATION_NAMES
                .iter()
                .all(|name| env::var_os(name).is_none()) =>
        {
            return Ok(false);
        }
        Err(env::VarError::NotPresent) => {
            return Err(
                format!("{MODE} is required when enterprise identity is configured").into(),
            );
        }
        Err(error) => return Err(error.into()),
    };
    if mode != "https-verifier" {
        return Err(format!("{MODE} must be https-verifier").into());
    }
    Ok(true)
}

fn local_production_configs() -> Result<
    (
        LocalDeliveryAdapterConfig,
        LocalPublicationAdapterConfig,
        RepositoryScope,
    ),
    Box<dyn std::error::Error>,
> {
    let scope = RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(required_environment("WWC_SERVER_ORGANIZATION_ID")?),
        workspace_id: WorkspaceId(required_environment("WWC_SERVER_WORKSPACE_ID")?),
        project_id: ProjectId(required_environment("WWC_SERVER_PROJECT_ID")?),
        repository_id: RepositoryId(required_environment("WWC_SERVER_REPOSITORY_ID")?),
    };
    let delivery = LocalDeliveryAdapterConfig::new(
        PathBuf::from(required_environment("WWC_SERVER_REPOSITORY_ROOT")?),
        scope.clone(),
    );
    let requester_ids = comma_separated_environment("PUBLICATION_REQUESTERS")?;
    let approvers = comma_separated_environment("PUBLICATION_APPROVERS")?
        .into_iter()
        .map(UserId)
        .collect();
    let publication = LocalPublicationAdapterConfig::try_new(
        scope.clone(),
        required_environment("GITHUB_REPOSITORY")?,
        CredentialReferenceId(required_environment("GITHUB_CREDENTIAL_REFERENCE_ID")?),
        required_environment("GITHUB_API_BASE_URL")?,
        PathBuf::from(required_environment("SECRET_DIRECTORY")?),
        requester_ids,
        approvers,
        required_environment("PUBLICATION_APPROVAL_MAX_AGE_MILLIS")?.parse()?,
    )?;
    Ok((delivery, publication, scope))
}

fn environment_config() -> Result<ServerConfig, Box<dyn std::error::Error>> {
    let bind_address: SocketAddr = required_environment("WWC_SERVER_BIND")?.parse()?;
    let public_url = required_environment("WWC_SERVER_PUBLIC_URL")?;
    let data_directory = PathBuf::from(required_environment("WWC_SERVER_DATA_DIRECTORY")?);
    let allowed_origins: BTreeSet<String> = required_environment("WWC_SERVER_ALLOWED_ORIGINS")?
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(str::to_owned)
        .collect();
    let certificate = env::var_os("WWC_SERVER_TLS_CERTIFICATE").map(PathBuf::from);
    let private_key = env::var_os("WWC_SERVER_TLS_PRIVATE_KEY").map(PathBuf::from);
    let tls = match (certificate, private_key) {
        (None, None) => ServerTls::Disabled,
        (Some(certificate_path), Some(private_key_path)) => ServerTls::Pem {
            certificate_path,
            private_key_path,
        },
        _ => return Err("both TLS certificate and private key must be configured".into()),
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

fn required_environment(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is required").into())
}

fn required_environment_or(
    name: &str,
    default: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    match env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        Ok(_) | Err(env::VarError::NotPresent) => Ok(default.to_owned()),
        Err(error) => Err(error.into()),
    }
}

fn optional_i64(name: &str, default: i64) -> Result<i64, Box<dyn std::error::Error>> {
    match env::var(name) {
        Ok(value) => Ok(value.parse()?),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn comma_separated_environment(name: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let values = required_environment(name)?
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return Err(format!("{name} must contain at least one identity").into());
    }
    Ok(values)
}

fn optional_duration_seconds(
    name: &str,
    default: u64,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let seconds = match env::var(name) {
        Ok(value) => value.parse::<u64>()?,
        Err(env::VarError::NotPresent) => default,
        Err(error) => return Err(error.into()),
    };
    Ok(Duration::from_secs(seconds))
}
