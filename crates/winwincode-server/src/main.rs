// SPDX-License-Identifier: Apache-2.0

#![recursion_limit = "256"]

use std::collections::BTreeSet;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use winwincode_api::generated::{
    OrganizationScope, OrganizationScopeKind, ProjectScope, ProjectScopeKind, Scope,
    WorkspaceScope, WorkspaceScopeKind,
};
use winwincode_control_plane::{
    CollaborationService, ControlPlane, ControlPlaneConfig, DurableWorkerInteractionOutbound,
    LocalDeliveryAdapterConfig, LocalPublicationAdapterConfig, ProductSessionExecutionApplication,
    ProductSessionExecutionConfig,
};
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, ProjectId, RepositoryId, RepositoryScope,
    RepositoryScopeKind, UserAccount, UserId, WorkerId, WorkerInstanceId, WorkspaceId,
};
use winwincode_execution_port::{
    action_enforcement::{ActionEnforcementIssuer, ActionEnforcementSigningKey},
    generated::ExecutionPortMessage,
    transport::ExecutionPortCore,
};
use winwincode_server::{
    AuthSessionBootstrap, AuthSessionConfig, ClientExchangeApplication, ClientExchangeConfig,
    ClientExchangePort, DurableEventHub, DurableEventHubConfig, DurableEventPublisher,
    GeneratedContractDispatcher, ProductionRemoteWorkerExchange, RemoteWorkerExchangePort,
    RepositoryRuntimeScheduler, RequestAuthenticator, ServerConfig, ServerExecutionPortCore,
    ServerTls, SqliteAuthSessionManager, StandaloneControlPlaneApplication, UserAccountService,
    WorkerSessionRemoteAuthenticator, start_server_with_remote_worker,
};
use winwincode_storage::{
    ProductStateStorage, SqliteStorage, WorkerOutboundQueueConfig, WorkerPoolId,
    WorkerRegistryScope,
};

const SERVER_TOKIO_WORKER_STACK_BYTES: usize = 32 * 1024 * 1024;

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

struct ProductionStartup {
    config: ServerConfig,
    delivery: LocalDeliveryAdapterConfig,
    publication: LocalPublicationAdapterConfig,
    repository_scope: RepositoryScope,
    execution_config: ProductSessionExecutionConfig,
    auth_bootstrap: AuthSessionBootstrap,
    auth_config: AuthSessionConfig,
}

struct ProductionApplicationComposition {
    config: ServerConfig,
    repository_scope: RepositoryScope,
    auth_sessions: Arc<SqliteAuthSessionManager>,
    owner: Option<UserAccount>,
    application: StandaloneControlPlaneApplication,
}

fn load_production_startup() -> Result<ProductionStartup, Box<dyn std::error::Error>> {
    let config = environment_config()?;
    let (delivery, publication, repository_scope) = local_production_configs()?;
    let execution_config = ProductSessionExecutionConfig::try_new(
        repository_scope.clone(),
        required_environment("WWC_SERVER_CHECKOUT_REVISION")?,
        required_environment_or("WWC_SERVER_EXECUTION_PROFILE", "codex-chat")?,
        optional_i64("WWC_SERVER_MAX_RUNTIME_SECONDS", 3_600)?,
        optional_i64("WWC_SERVER_MAX_ARTIFACT_BYTES", 1_073_741_824)?,
    )?;
    let bootstrap_proof = required_environment("WWC_SERVER_BOOTSTRAP_PROOF")?;
    let auth_bootstrap = AuthSessionBootstrap::new(bootstrap_proof)?;
    // Community local loopback defaults to unlocked access; password auth is
    // required for lock mode or any non-loopback listener.
    let local_open = match env::var("WWC_SERVER_AUTH_MODE") {
        Ok(mode) if mode == "local-open" => true,
        Ok(mode) if mode == "password" => false,
        Ok(_) => return Err("WWC_SERVER_AUTH_MODE must be local-open or password".into()),
        Err(env::VarError::NotPresent) => config.bind_address().ip().is_loopback(),
        Err(error) => return Err(error.into()),
    };
    let auth_config = AuthSessionConfig::new(
        optional_duration_seconds("WWC_SERVER_BOOTSTRAP_WINDOW_SECONDS", 10 * 60)?,
        optional_duration_seconds("WWC_SERVER_SESSION_TTL_SECONDS", 8 * 60 * 60)?,
    )?
    .with_local_open(local_open);
    Ok(ProductionStartup {
        config,
        delivery,
        publication,
        repository_scope,
        execution_config,
        auth_bootstrap,
        auth_config,
    })
}

/// Opens the durable account authority, session manager, and resolves the
/// first Owner account behind the initialization marker.
type OpenedAccountsAuthority = (Arc<SqliteAuthSessionManager>, Option<UserAccount>);

#[allow(clippy::type_complexity)]
fn open_accounts_authority(
    config: &ServerConfig,
    repository_scope: &RepositoryScope,
    auth_bootstrap: AuthSessionBootstrap,
    auth_config: AuthSessionConfig,
) -> Result<OpenedAccountsAuthority, Box<dyn std::error::Error>> {
    let accounts = Arc::new(UserAccountService::open(config.data_directory())?);
    let auth_sessions = Arc::new(SqliteAuthSessionManager::open(
        config.data_directory().join("auth-sessions"),
        vec![auth_bootstrap],
        local_session_authority(repository_scope),
        auth_config,
        Arc::clone(&accounts),
        None,
    )?);
    Ok((
        Arc::clone(&auth_sessions),
        resolved_owner(&auth_sessions, &accounts)?,
    ))
}

/// The local Server exposes one configured repository, but the browser's
/// post-login shell also reads the canonical organization, project, and
/// request-pool streams for that repository. Keep those exact ancestor
/// scopes in the session authority so every generated request and
/// subscription is authorized by the same initialized Owner session.
fn local_session_authority(repository: &RepositoryScope) -> Vec<Scope> {
    vec![
        Scope::OrganizationScope(OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: repository.organization_id.clone(),
        }),
        Scope::WorkspaceScope(WorkspaceScope {
            kind: WorkspaceScopeKind::Workspace,
            organization_id: repository.organization_id.clone(),
            workspace_id: repository.workspace_id.clone(),
        }),
        Scope::ProjectScope(ProjectScope {
            kind: ProjectScopeKind::Project,
            organization_id: repository.organization_id.clone(),
            workspace_id: repository.workspace_id.clone(),
            project_id: repository.project_id.clone(),
        }),
        Scope::RepositoryScope(repository.clone()),
    ]
}

fn open_production_application(
    startup: ProductionStartup,
) -> Result<ProductionApplicationComposition, Box<dyn std::error::Error>> {
    let ProductionStartup {
        config,
        delivery,
        publication,
        repository_scope,
        execution_config,
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
    let storage = match SqliteStorage::open(config.data_directory()) {
        Ok(storage) => storage,
        Err(error) => {
            let _ = control_plane.shutdown();
            let _ = hub.close();
            return Err(Box::new(error));
        }
    };
    let (auth_sessions, owner) =
        open_accounts_authority(&config, &repository_scope, auth_bootstrap, auth_config)?;
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
    let application = compose_production_application(
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
        auth_sessions,
        owner,
        application,
    })
}

/// Resolves the durable first Owner account behind the initialization marker.
fn resolved_owner(
    auth_sessions: &SqliteAuthSessionManager,
    accounts: &UserAccountService,
) -> Result<Option<UserAccount>, Box<dyn std::error::Error>> {
    let Some(user_id) = auth_sessions.initialized_owner() else {
        return Ok(None);
    };
    let account = accounts
        .find(&user_id)?
        .ok_or("durable initialization marker names a missing Owner account")?;
    Ok(Some(account))
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
        auth_sessions,
        owner,
        application,
    } = composition;

    let action_signing_key = configured_action_signing_key()?;
    let delegate = ProductSessionExecutionApplication::new_with_action_issuer(
        DeviceModelBoundary,
        ActionEnforcementIssuer::new(action_signing_key.clone()),
    );
    let execution_port =
        ServerExecutionPortCore::from_application(&application, repository_scope.clone(), delegate);
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
    let worker_pool_id = WorkerPoolId(required_environment_or(
        "WWC_SERVER_WORKER_POOL_ID",
        "wpl_00000000000000000000000001",
    )?);
    let scheduler = RepositoryRuntimeScheduler::from_application(
        &application,
        repository_scope.clone(),
        worker_id.clone(),
        worker_instance_id.clone(),
        scheduler_generation,
        // ponytail: execution leases have a fixed deadline; tasks over 15 minutes
        // need an explicit longer lease until protocol-level renewal is implemented.
        optional_duration_seconds("WWC_SERVER_EXECUTION_LEASE_SECONDS", 900)?,
    )?
    .with_admission_identity(
        owner.as_ref().map(|owner| owner.user_id.clone()),
        worker_pool_id,
    )?;
    Box::pin(run_remote_composition(RemoteRuntimeComposition {
        config,
        repository_scope,
        auth_sessions,
        application,
        scheduler,
        execution_port,
    }))
    .await
}

/// Model payloads and Provider credentials stay in the Device process.
struct DeviceModelBoundary;
impl winwincode_control_plane::DurableExecutionPortDelegate for DeviceModelBoundary {
    fn accept(
        &mut self,
        _context: winwincode_control_plane::DurableExecutionPortContext<'_>,
        _message: winwincode_control_plane::DurableExecutionPortSupplement<'_>,
    ) -> Result<Vec<ExecutionPortMessage>, winwincode_control_plane::DurableExecutionPortError>
    {
        Err(winwincode_control_plane::DurableExecutionPortError::UnsupportedMessage)
    }
}

struct RemoteRuntimeComposition<Core> {
    config: ServerConfig,
    repository_scope: RepositoryScope,
    auth_sessions: Arc<SqliteAuthSessionManager>,
    application: StandaloneControlPlaneApplication,
    scheduler: RepositoryRuntimeScheduler,
    execution_port: Core,
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
        auth_sessions,
        application,
        scheduler,
        execution_port,
    } = composition;
    let remote_worker_scope = WorkerRegistryScope::Repository {
        organization_id: repository_scope.organization_id.clone(),
        workspace_id: repository_scope.workspace_id.clone(),
        project_id: repository_scope.project_id.clone(),
        repository_id: repository_scope.repository_id.clone(),
    };
    let session_authenticator = WorkerSessionRemoteAuthenticator::new(
        config.data_directory().to_path_buf(),
        remote_worker_scope,
    );
    let remote_authenticator = Arc::new(session_authenticator);
    let exchange: Arc<dyn RemoteWorkerExchangePort> =
        Arc::new(ProductionRemoteWorkerExchange::new(
            config.data_directory(),
            remote_authenticator,
            scheduler,
            execution_port,
        ));
    let client_exchange: Arc<dyn ClientExchangePort> = Arc::new(
        ClientExchangeApplication::open(config.data_directory(), &ClientExchangeConfig::default())
            .map_err(|error| error.to_string())?,
    );
    let api = Arc::new(GeneratedContractDispatcher::new(Arc::new(application)));
    let authenticator: Arc<dyn RequestAuthenticator> = auth_sessions.clone();
    serve_remote_runtime(
        config,
        auth_sessions,
        authenticator,
        api,
        exchange,
        client_exchange,
    )
    .await
}

async fn serve_remote_runtime(
    config: ServerConfig,
    auth_sessions: Arc<SqliteAuthSessionManager>,
    authenticator: Arc<dyn RequestAuthenticator>,
    api: Arc<GeneratedContractDispatcher>,
    exchange: Arc<dyn RemoteWorkerExchangePort>,
    client_exchange: Arc<dyn ClientExchangePort>,
) -> Result<(), Box<dyn std::error::Error>> {
    let running = start_server_with_remote_worker(
        config,
        auth_sessions,
        authenticator,
        api,
        Some(exchange),
        Some(client_exchange),
    )
    .await?;
    tokio::signal::ctrl_c().await?;
    running.shutdown().await?;
    Ok(())
}

fn configured_action_signing_key() -> Result<ActionEnforcementSigningKey, Box<dyn std::error::Error>>
{
    Ok(ActionEnforcementSigningKey::from_bytes(parse_hex_key(
        &required_environment_or("WWC_SERVER_ACTION_SIGNING_KEY_HEX", &"1f".repeat(32))?,
    )?)?)
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

fn compose_production_application(
    config: &ServerConfig,
    control_plane: ControlPlane,
    storage: SqliteStorage,
    worker_outbound: DurableWorkerInteractionOutbound,
    hub: Arc<DurableEventHub>,
    execution_config: ProductSessionExecutionConfig,
) -> Result<StandaloneControlPlaneApplication, Box<dyn std::error::Error>> {
    let collaboration = Arc::new(CollaborationService::new(SqliteStorage::open(
        config.data_directory(),
    )?));
    let application = StandaloneControlPlaneApplication::new_with_collaboration(
        control_plane,
        storage,
        worker_outbound,
        hub,
        collaboration,
        execution_config,
    )?;
    Ok(application)
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
    let config = ServerConfig::new(
        bind_address,
        public_url,
        tls,
        allowed_origins,
        data_directory,
        Duration::from_secs(30),
    )?;
    match env::var("WWC_SERVER_PREVIEW_PUBLIC_URL") {
        Ok(origin) if !origin.is_empty() => Ok(config.with_preview_public_url(origin)?),
        Ok(_) | Err(env::VarError::NotPresent) => Ok(config),
        Err(error) => Err(error.into()),
    }
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
