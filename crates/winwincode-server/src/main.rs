#![recursion_limit = "256"]
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use winwincode_api::generated::{
    OrganizationScope, OrganizationScopeKind, ProjectScope, ProjectScopeKind, Scope,
    WorkspaceScope, WorkspaceScopeKind,
};
use winwincode_control_plane::{
    CollaborationInboxService, CollaborationService, ControlPlane, ControlPlaneConfig,
    DurableCollaborationInboxSource, DurableWorkerInteractionOutbound, LocalDeliveryAdapterConfig,
    LocalOwnerCollaborationInboxAuthority, LocalPublicationAdapterConfig,
    ProductSessionExecutionApplication, ProductSessionExecutionConfig,
};
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, ProjectId, RepositoryId, RepositoryScope,
    RepositoryScopeKind, UserAccount, UserId, WorkerId, WorkerInstanceId, WorkspaceId,
};
use winwincode_execution_port::{generated::ExecutionPortMessage, transport::ExecutionPortCore};
use winwincode_server::{
    AuthSessionBootstrap, AuthSessionConfig, ClientExchangeApplication, ClientExchangeConfig,
    ClientExchangePort, DurableEventHub, DurableEventHubConfig, DurableEventPublisher,
    GeneratedContractDispatcher, ProductionRemoteWorkerExchange, RemoteWorkerExchangePort,
    RepositoryRuntimeScheduler, RequestAuthenticator, ServerConfig, ServerExecutionPortCore,
    SqliteAuthSessionManager, StandaloneControlPlaneApplication, UserAccountService,
    start_server_with_remote_worker,
};
use winwincode_storage::{
    ArtifactStore, LocalArtifactObjectStore, ProductStateStorage, SqliteStorage,
    WorkerOutboundQueueConfig, WorkerPoolId,
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
    publication: Option<LocalPublicationAdapterConfig>,
    repository_scope: RepositoryScope,
    execution_config: ProductSessionExecutionConfig,
    auth_bootstrap: Option<AuthSessionBootstrap>,
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
        required_environment_or("WWC_SERVER_CHECKOUT_REVISION", "HEAD")?,
        required_environment_or("WWC_SERVER_EXECUTION_PROFILE", "codex-chat")?,
        execution_runtime_seconds()?,
        optional_i64("WWC_SERVER_MAX_ARTIFACT_BYTES", 1_073_741_824)?,
    )?;
    let local_open = match env::var("WWC_SERVER_AUTH_MODE") {
        Ok(mode) if mode == "local-open" => true,
        Ok(mode) if mode == "password" => false,
        Ok(_) => return Err("WWC_SERVER_AUTH_MODE must be local-open or password".into()),
        Err(env::VarError::NotPresent) => !config.remote(),
        Err(error) => return Err(error.into()),
    };
    if local_open && config.remote() {
        return Err("local-open is restricted to a local HTTP loopback entry".into());
    }
    let auth_bootstrap = match env::var("WWC_SERVER_BOOTSTRAP_PROOF") {
        Ok(proof) if !proof.is_empty() => Some(AuthSessionBootstrap::new(proof)?),
        _ => None,
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

type OpenedAccountsAuthority = (Arc<SqliteAuthSessionManager>, Option<UserAccount>);

#[allow(clippy::type_complexity)]
fn open_accounts_authority(
    config: &ServerConfig,
    repository_scope: &RepositoryScope,
    auth_bootstrap: Option<AuthSessionBootstrap>,
    auth_config: AuthSessionConfig,
) -> Result<OpenedAccountsAuthority, Box<dyn std::error::Error>> {
    let accounts = Arc::new(UserAccountService::open(config.data_directory())?);
    let auth_sessions = Arc::new(SqliteAuthSessionManager::open(
        config.data_directory().join("auth-sessions"),
        auth_bootstrap.into_iter().collect(),
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
    let control_plane = match ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(config.data_directory()),
        Box::new(DurableEventPublisher::new(Arc::clone(&hub))),
        delivery,
    ) {
        Ok(control_plane) => control_plane,
        Err(error) => {
            let _ = hub.close();
            return Err(Box::new(error));
        }
    };
    let mut control_plane = control_plane;
    if let Some(publication) = publication {
        control_plane.install_local_publication_adapters(publication)?;
    }
    let execution_artifact_objects =
        LocalArtifactObjectStore::open(config.data_directory().join("artifacts"))?;
    let execution_artifact_store = ArtifactStore::open(
        config.data_directory().join("artifact-catalog"),
        Box::new(execution_artifact_objects),
    )?;
    if let Err(error) = control_plane.install_artifact_store(execution_artifact_store) {
        let _ = control_plane.shutdown();
        let _ = hub.close();
        return Err(Box::new(error));
    }
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
        owner
            .as_ref()
            .ok_or("local Owner account is not initialized")?,
    )?;
    Ok(ProductionApplicationComposition {
        config,
        repository_scope,
        auth_sessions,
        owner,
        application,
    })
}

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
    let (sessions, owner) = open_accounts_authority(
        &startup.config,
        &startup.repository_scope,
        startup.auth_bootstrap.clone(),
        startup.auth_config,
    )?;
    if owner.is_none() {
        winwincode_server::wait_for_owner_initialization(startup.config.clone(), sessions).await?;
    } else {
        drop(sessions);
    }
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

    let delegate = ProductSessionExecutionApplication::new(DeviceModelBoundary);
    let lease_duration = optional_duration_seconds("WWC_SERVER_EXECUTION_LEASE_SECONDS", 900)?;
    let execution_port = ServerExecutionPortCore::from_application(
        &application,
        repository_scope.clone(),
        delegate,
        lease_duration,
    )?;
    let worker_id = WorkerId(required_environment_or(
        "WWC_SERVER_WORKER_ID",
        "wrk_00000000000000000000000001",
    )?);
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
        lease_duration,
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
    let exchange: Arc<dyn RemoteWorkerExchangePort> =
        Arc::new(ProductionRemoteWorkerExchange::new(
            config.data_directory(),
            repository_scope,
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
    owner: &UserAccount,
) -> Result<StandaloneControlPlaneApplication, Box<dyn std::error::Error>> {
    let collaboration = Arc::new(CollaborationService::new(SqliteStorage::open(
        config.data_directory(),
    )?));
    let artifact_objects =
        LocalArtifactObjectStore::open(config.data_directory().join("artifacts"))?;
    let artifact_store = ArtifactStore::open(
        config
            .data_directory()
            .join("artifact-catalog-collaboration"),
        Box::new(artifact_objects),
    )?;
    let page_annotations = Arc::new(Mutex::new(
        CollaborationInboxService::new_with_artifact_store(
            Box::new(SqliteStorage::open(config.data_directory())?),
            Box::new(DurableCollaborationInboxSource::new(Box::new(
                SqliteStorage::open(config.data_directory())?,
            ))),
            Box::new(LocalOwnerCollaborationInboxAuthority::new(
                Box::new(SqliteStorage::open(config.data_directory())?),
                owner.user_id.clone(),
            )),
            artifact_store,
        ),
    ));
    let application =
        StandaloneControlPlaneApplication::new_with_collaboration_and_page_annotations(
            control_plane,
            storage,
            worker_outbound,
            hub,
            collaboration,
            page_annotations,
            execution_config,
        )?;
    Ok(application)
}

fn local_production_configs() -> Result<
    (
        LocalDeliveryAdapterConfig,
        Option<LocalPublicationAdapterConfig>,
        RepositoryScope,
    ),
    Box<dyn std::error::Error>,
> {
    let scope = RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(required_environment_or(
            "WWC_SERVER_ORGANIZATION_ID",
            "org_00000000000000000000000000",
        )?),
        workspace_id: WorkspaceId(required_environment_or(
            "WWC_SERVER_WORKSPACE_ID",
            "wsp_00000000000000000000000000",
        )?),
        project_id: ProjectId(required_environment_or(
            "WWC_SERVER_PROJECT_ID",
            "prj_00000000000000000000000000",
        )?),
        repository_id: RepositoryId(required_environment_or(
            "WWC_SERVER_REPOSITORY_ID",
            "rep_00000000000000000000000000",
        )?),
    };
    let delivery = LocalDeliveryAdapterConfig::new(
        env::var_os("WWC_SERVER_REPOSITORY_ROOT")
            .map_or_else(env::current_dir, |root| Ok(PathBuf::from(root)))?,
        scope.clone(),
    )
    .with_execution_limits(
        execution_runtime_seconds()?,
        optional_i64("WWC_SERVER_MAX_ARTIFACT_BYTES", 1_073_741_824)?,
    );
    let publication = if [
        "GITHUB_REPOSITORY",
        "GITHUB_CREDENTIAL_REFERENCE_ID",
        "GITHUB_API_BASE_URL",
        "SECRET_DIRECTORY",
        "PUBLICATION_REQUESTERS",
        "PUBLICATION_APPROVERS",
        "PUBLICATION_APPROVAL_MAX_AGE_MILLIS",
    ]
    .into_iter()
    .all(|name| env::var(name).is_ok_and(|value| !value.is_empty()))
    {
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
        Some(publication)
    } else {
        None
    };
    Ok((delivery, publication, scope))
}

fn environment_config() -> Result<ServerConfig, Box<dyn std::error::Error>> {
    let bind_address: SocketAddr =
        required_environment_or("WWC_SERVER_BIND", "127.0.0.1:8443")?.parse()?;
    let public_url = required_environment_or("WWC_SERVER_PUBLIC_URL", "http://127.0.0.1:8443")?;
    let advertised_endpoint: http::Uri = public_url.parse()?;
    let public_host = advertised_endpoint
        .host()
        .ok_or("WWC_SERVER_PUBLIC_URL requires a host")?;
    let local_public_host = public_host == "localhost"
        || public_host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|host| host.is_loopback());
    if advertised_endpoint.scheme_str() != Some("https") && !local_public_host {
        return Err("a non-loopback WWC_SERVER_PUBLIC_URL requires HTTPS".into());
    }
    let data_directory = match env::var_os("WWC_SERVER_DATA_DIRECTORY") {
        Some(directory) => PathBuf::from(directory),
        None => match env::var_os("WINWINCODE_HOME") {
            Some(directory) => PathBuf::from(directory).join("server"),
            None => PathBuf::from(required_environment("HOME")?).join(".winwincode/server"),
        },
    };
    let allowed_origins: BTreeSet<String> = required_environment_or(
        "WWC_SERVER_ALLOWED_ORIGINS",
        "http://127.0.0.1:8080,http://localhost:8080",
    )?
    .split(',')
    .map(str::trim)
    .filter(|origin| !origin.is_empty())
    .map(str::to_owned)
    .collect();
    let mut config = ServerConfig::new(
        bind_address,
        public_url,
        allowed_origins,
        data_directory,
        Duration::from_secs(30),
    )?;
    match (
        env::var_os("WWC_SERVER_TLS_CERTIFICATE"),
        env::var_os("WWC_SERVER_TLS_PRIVATE_KEY"),
    ) {
        (Some(certificate), Some(private_key)) => {
            config = config.with_tls_pem(certificate.into(), private_key.into())?;
        }
        (None, None) => {}
        _ => {
            return Err(
                "both WWC_SERVER_TLS_CERTIFICATE and WWC_SERVER_TLS_PRIVATE_KEY are required"
                    .into(),
            );
        }
    }
    match env::var("WWC_SERVER_TLS_PROXY") {
        Ok(value) if value == "loopback" => {
            config = config.with_loopback_tls_proxy()?;
        }
        Ok(_) => return Err("WWC_SERVER_TLS_PROXY must be loopback".into()),
        Err(env::VarError::NotPresent) => {}
        Err(error) => return Err(error.into()),
    }
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

fn execution_runtime_seconds() -> Result<Option<i64>, Box<dyn std::error::Error>> {
    let value = required_environment_or("WWC_SERVER_MAX_RUNTIME_SECONDS", "3600")?;
    parse_execution_runtime_seconds(&value)
}

fn parse_execution_runtime_seconds(value: &str) -> Result<Option<i64>, Box<dyn std::error::Error>> {
    if value == "unlimited" {
        return Ok(None);
    }
    let seconds: i64 = value.parse()?;
    if !(1..=604_800).contains(&seconds) {
        return Err("WWC_SERVER_MAX_RUNTIME_SECONDS must be 1..604800 or unlimited".into());
    }
    Ok(Some(seconds))
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

#[cfg(test)]
mod execution_runtime_tests {
    use super::parse_execution_runtime_seconds;

    #[test]
    fn runtime_policy_requires_an_explicit_finite_value_or_unlimited() {
        assert_eq!(parse_execution_runtime_seconds("unlimited").unwrap(), None);
        assert_eq!(parse_execution_runtime_seconds("3600").unwrap(), Some(3600));
        for value in ["", "0", "-1", "604801", "null", "UNLIMITED"] {
            assert!(parse_execution_runtime_seconds(value).is_err(), "{value}");
        }
    }
}
