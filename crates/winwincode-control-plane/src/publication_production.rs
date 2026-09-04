// SPDX-License-Identifier: Apache-2.0

//! Production Publication policy authority and provider registry.

use std::{
    fmt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use winwincode_api::generated::{
    CommandEnvelope, CommandName, PublicationPublishCommand, PublicationPublishCompletedResponse,
    PublicationPublishCompletedResponseCommand, PublicationPublishCompletedResponseOutcome,
    PublicationTarget as ApiPublicationTarget, PublicationTargetProvider, Scope,
};
use winwincode_domain::RepositoryScope;
use winwincode_domain::{
    CredentialReferenceId, Revision, SchemaVersion, ServiceAccountId, SystemActorId, UserId,
};
use winwincode_publication::{
    CredentialResolutionError, GitHubAdapterConfig, GitHubCredential, GitHubCredentialResolver,
    GitHubPublicationAdapter, PolicyPermission, PublicationAuthorization,
    PublicationEnterpriseAttribution, PublicationPolicyEvidence, PublicationPolicyOrigin,
    PublicationPort, PublicationReadLedger, PublicationRequester, RepositoryPolicyScope,
    RepositoryPublicationPolicy,
};
use winwincode_storage::{ProductStateStorage, SqliteStorage};

use crate::{
    ControlPlane, CredentialReferenceErrorKind, CredentialReferenceService,
    CredentialSecretResolutionError, EventPublisher, LocalDeliveryAdapterConfig,
    LocalSecretStoreAdapter, PublicationCommandError, PublicationEnterpriseQuotaSaga,
    PublicationEnterpriseUsageReconciler, SecretStoreErrorKind, StartError, command_receipt,
    publication_application::{checked_response, publication_projection},
    publication_enterprise_quota::publication_quota_requested_at,
    strongflow_projection::load_current_publication_read,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Trusted input assembled after the current Delivery, candidate, verdict,
/// approval, and review-package Artifact have been resolved.
pub struct PublicationAuthorityRequest<'request> {
    command: &'request PublicationPublishCommand,
    authorization: &'request PublicationAuthorization,
    attribution: &'request PublicationEnterpriseAttribution,
    observed_at_millis: u64,
}

impl<'request> PublicationAuthorityRequest<'request> {
    pub(crate) const fn new(
        command: &'request PublicationPublishCommand,
        authorization: &'request PublicationAuthorization,
        attribution: &'request PublicationEnterpriseAttribution,
        observed_at_millis: u64,
    ) -> Self {
        Self {
            command,
            authorization,
            attribution,
            observed_at_millis,
        }
    }

    #[must_use]
    pub const fn command(&self) -> &PublicationPublishCommand {
        self.command
    }

    #[must_use]
    pub const fn authorization(&self) -> &PublicationAuthorization {
        self.authorization
    }

    #[must_use]
    pub const fn attribution(&self) -> &PublicationEnterpriseAttribution {
        self.attribution
    }

    #[must_use]
    pub const fn observed_at_millis(&self) -> u64 {
        self.observed_at_millis
    }
}

/// Opaque policy facts consumed by the Publication coordinator.
pub struct PublicationAuthorityFacts {
    authorization: PublicationAuthorization,
    attribution: PublicationEnterpriseAttribution,
    policy: RepositoryPublicationPolicy,
    evidence: PublicationPolicyEvidence,
    origin: PublicationPolicyOrigin,
}

impl PublicationAuthorityFacts {
    #[must_use]
    pub const fn authorization(&self) -> &PublicationAuthorization {
        &self.authorization
    }

    #[must_use]
    pub const fn attribution(&self) -> &PublicationEnterpriseAttribution {
        &self.attribution
    }

    #[must_use]
    pub const fn policy(&self) -> &RepositoryPublicationPolicy {
        &self.policy
    }

    #[must_use]
    pub const fn evidence(&self) -> &PublicationPolicyEvidence {
        &self.evidence
    }

    #[must_use]
    pub const fn origin(&self) -> &PublicationPolicyOrigin {
        &self.origin
    }
}

/// Stable failure before a Publication intent can be written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationAuthorityErrorKind {
    InvalidConfiguration,
    TrustedFactsUnavailable,
}

/// Secret-safe authority failure.
#[derive(Debug)]
pub struct PublicationAuthorityError {
    kind: PublicationAuthorityErrorKind,
    message: &'static str,
}

impl PublicationAuthorityError {
    const fn invalid_configuration() -> Self {
        Self {
            kind: PublicationAuthorityErrorKind::InvalidConfiguration,
            message: "Publication authority configuration is invalid",
        }
    }

    pub(crate) const fn trusted_facts_unavailable() -> Self {
        Self {
            kind: PublicationAuthorityErrorKind::TrustedFactsUnavailable,
            message: "Current Publication authority facts are unavailable",
        }
    }

    #[must_use]
    pub const fn kind(&self) -> PublicationAuthorityErrorKind {
        self.kind
    }
}

impl fmt::Display for PublicationAuthorityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for PublicationAuthorityError {}

/// Only application-owned prepared facts can cross this policy seam.
pub trait PublicationAuthorityPort: Send {
    /// Resolves one exact generated command into sealed policy facts.
    ///
    /// # Errors
    ///
    /// Rejects foreign scope, target, Delivery, candidate, approval time, or
    /// malformed startup policy.
    fn resolve(
        &mut self,
        request: PublicationAuthorityRequest<'_>,
    ) -> Result<PublicationAuthorityFacts, PublicationAuthorityError>;
}

/// Startup-only policy configuration for one local repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalPublicationAuthorityConfig {
    policy_scope: RepositoryPolicyScope,
    repository: String,
    allowed_requesters: Vec<PublicationRequester>,
    allowed_approvers: Vec<UserId>,
    max_approval_age_millis: u64,
    origin_component: String,
}

impl LocalPublicationAuthorityConfig {
    /// Builds one closed allow policy from process configuration rather than
    /// request fields.
    ///
    /// # Errors
    ///
    /// Rejects an empty principal set, malformed repository, invalid lifetime,
    /// or non-portable audit origin.
    pub fn try_new(
        policy_scope: RepositoryPolicyScope,
        repository: impl Into<String>,
        allowed_requesters: Vec<PublicationRequester>,
        allowed_approvers: Vec<UserId>,
        max_approval_age_millis: u64,
        origin_component: impl Into<String>,
    ) -> Result<Self, PublicationAuthorityError> {
        let repository = repository.into();
        let origin_component = origin_component.into();
        RepositoryPublicationPolicy::try_new(
            policy_scope.clone(),
            repository.clone(),
            allowed_requesters.clone(),
            Vec::new(),
            allowed_approvers.clone(),
            Vec::new(),
            PolicyPermission::Allow,
            true,
            PolicyPermission::Allow,
            max_approval_age_millis,
        )
        .map_err(|_| PublicationAuthorityError::invalid_configuration())?;
        PublicationPolicyOrigin::local(&origin_component)
            .map_err(|_| PublicationAuthorityError::invalid_configuration())?;
        Ok(Self {
            policy_scope,
            repository,
            allowed_requesters,
            allowed_approvers,
            max_approval_age_millis,
            origin_component,
        })
    }
}

/// Local policy authority over already-sealed Delivery and Artifact facts.
pub struct LocalPublicationAuthority {
    policy: RepositoryPublicationPolicy,
    origin: PublicationPolicyOrigin,
}

impl LocalPublicationAuthority {
    /// Freezes the startup configuration into one immutable policy.
    ///
    /// # Errors
    ///
    /// Returns a stable configuration failure if any policy field is invalid.
    pub fn open(
        config: LocalPublicationAuthorityConfig,
    ) -> Result<Self, PublicationAuthorityError> {
        let policy = RepositoryPublicationPolicy::try_new(
            config.policy_scope,
            config.repository,
            config.allowed_requesters,
            Vec::new(),
            config.allowed_approvers,
            Vec::new(),
            PolicyPermission::Allow,
            true,
            PolicyPermission::Allow,
            config.max_approval_age_millis,
        )
        .map_err(|_| PublicationAuthorityError::invalid_configuration())?;
        let origin = PublicationPolicyOrigin::local(&config.origin_component)
            .map_err(|_| PublicationAuthorityError::invalid_configuration())?;
        Ok(Self { policy, origin })
    }
}

impl PublicationAuthorityPort for LocalPublicationAuthority {
    fn resolve(
        &mut self,
        request: PublicationAuthorityRequest<'_>,
    ) -> Result<PublicationAuthorityFacts, PublicationAuthorityError> {
        let command = request.command();
        let authorization = request.authorization();
        let attribution = request.attribution();
        let scope = RepositoryPolicyScope::try_new(
            command.scope.organization_id.clone(),
            command.scope.workspace_id.clone(),
            command.scope.project_id.clone(),
            command.scope.repository_id.clone(),
        )
        .map_err(|_| PublicationAuthorityError::trusted_facts_unavailable())?;
        if self.policy.scope() != &scope
            || command.payload.delivery_id != *authorization.binding().delivery_id()
            || command.payload.candidate_digest != *authorization.candidate_digest()
            || command.payload.target.repository.0 != authorization.target().repository()
            || command.payload.target.base_branch != authorization.target().base_branch()
            || command.payload.target.head_repository.0 != authorization.target().head_repository()
            || command.payload.target.head_branch != authorization.target().head_branch()
            || command.payload.target.provider != PublicationTargetProvider::Github
            || request.observed_at_millis() < authorization.approved_at_millis()
            || request.observed_at_millis() > MAX_SAFE_INTEGER
            || attribution.organization_id() != scope.organization_id()
            || attribution.workspace_id() != scope.workspace_id()
            || attribution.project_id() != scope.project_id()
            || attribution.repository_id() != scope.repository_id()
            || attribution.delivery_id() != authorization.binding().delivery_id()
            || !matches!(
                &command.actor,
                winwincode_api::generated::Actor::UserActor(actor)
                    if &actor.id == attribution.user_id()
            )
        {
            return Err(PublicationAuthorityError::trusted_facts_unavailable());
        }
        let evidence = PublicationPolicyEvidence::try_from_current_facts(
            authorization,
            true,
            true,
            request.observed_at_millis(),
        )
        .map_err(|_| PublicationAuthorityError::trusted_facts_unavailable())?;
        Ok(PublicationAuthorityFacts {
            authorization: authorization.clone(),
            attribution: attribution.clone(),
            policy: self.policy.clone(),
            evidence,
            origin: self.origin.clone(),
        })
    }
}

/// Stable provider-registry failure before a Publication intent is written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationProviderRegistryErrorKind {
    NotConfigured,
    PermissionDenied,
    Unavailable,
}

/// Secret-safe provider-registry failure.
#[derive(Debug)]
pub struct PublicationProviderRegistryError {
    kind: PublicationProviderRegistryErrorKind,
    message: &'static str,
}

impl PublicationProviderRegistryError {
    const fn new(kind: PublicationProviderRegistryErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    fn not_configured() -> Self {
        Self::new(
            PublicationProviderRegistryErrorKind::NotConfigured,
            "Publication provider is not configured",
        )
    }

    fn permission_denied() -> Self {
        Self::new(
            PublicationProviderRegistryErrorKind::PermissionDenied,
            "Publication provider credential is not permitted",
        )
    }

    fn unavailable() -> Self {
        Self::new(
            PublicationProviderRegistryErrorKind::Unavailable,
            "Publication provider is unavailable",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> PublicationProviderRegistryErrorKind {
        self.kind
    }
}

impl fmt::Display for PublicationProviderRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for PublicationProviderRegistryError {}

/// Chooses one preconfigured provider and validates its current Credential
/// reference before any Publication state is created.
pub trait PublicationProviderRegistry: Send {
    /// Resolves one exact target into a short-lived provider session.
    ///
    /// # Errors
    ///
    /// Rejects unsupported/foreign targets, missing or revoked Credential
    /// references, and unavailable secret storage.
    fn resolve(
        &mut self,
        target: &ApiPublicationTarget,
    ) -> Result<PublicationProviderSession, PublicationProviderRegistryError>;
}

/// A short-lived provider port bound to the single Credential secret snapshot
/// resolved by the registry.
pub struct PublicationProviderSession {
    port: Box<dyn PublicationPort>,
}

impl PublicationProviderSession {
    fn new(port: Box<dyn PublicationPort>) -> Self {
        Self { port }
    }

    /// Returns the provider port that owns this session's frozen Credential
    /// snapshot.
    #[must_use]
    pub fn port(&mut self) -> &mut dyn PublicationPort {
        self.port.as_mut()
    }
}

struct SessionGitHubCredentialResolver {
    credential_reference_id: CredentialReferenceId,
    secret: Vec<u8>,
}

impl GitHubCredentialResolver for SessionGitHubCredentialResolver {
    fn resolve(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<GitHubCredential, CredentialResolutionError> {
        if reference != &self.credential_reference_id {
            return Err(CredentialResolutionError::permission_denied());
        }
        GitHubCredential::try_new("github", &self.secret)
            .map_err(|_| CredentialResolutionError::unavailable())
    }
}

impl Drop for SessionGitHubCredentialResolver {
    fn drop(&mut self) {
        self.secret.fill(0);
    }
}

/// Production registry configuration for one GitHub repository.
#[derive(Clone, Debug)]
pub struct LocalGitHubProviderConfig {
    credential_reference_id: CredentialReferenceId,
    api_base_url: String,
    repository: String,
    scope: Scope,
}

impl LocalGitHubProviderConfig {
    /// Creates one registry entry from startup configuration.
    #[must_use]
    pub fn new(
        credential_reference_id: CredentialReferenceId,
        api_base_url: impl Into<String>,
        repository: impl Into<String>,
        scope: Scope,
    ) -> Self {
        Self {
            credential_reference_id,
            api_base_url: api_base_url.into(),
            repository: repository.into(),
            scope,
        }
    }
}

/// Single-entry production GitHub provider registry.
pub struct LocalPublicationProviderRegistry {
    credential_reference_id: CredentialReferenceId,
    repository: String,
    scope: Scope,
    credentials: Box<dyn PublicationCredentialSource>,
    adapter_config: GitHubAdapterConfig,
}

trait PublicationCredentialSource: Send {
    fn resolve(
        &mut self,
        scope: &Scope,
        credential_reference_id: &CredentialReferenceId,
    ) -> Result<crate::ResolvedSecret, CredentialResolutionError>;
}

struct LocalPublicationCredentialSource {
    storage: SqliteStorage,
    secrets: LocalSecretStoreAdapter,
}

impl PublicationCredentialSource for LocalPublicationCredentialSource {
    fn resolve(
        &mut self,
        scope: &Scope,
        credential_reference_id: &CredentialReferenceId,
    ) -> Result<crate::ResolvedSecret, CredentialResolutionError> {
        CredentialReferenceService::new(&mut self.storage)
            .resolve_secret(&self.secrets, scope, credential_reference_id)
            .map_err(map_credential_error)
    }
}

/// Complete startup configuration for local Publication policy and provider
/// ownership.
#[derive(Clone, Debug)]
pub struct LocalPublicationAdapterConfig {
    authority: LocalPublicationAuthorityConfig,
    provider: LocalGitHubProviderConfig,
    secret_directory: PathBuf,
}

impl LocalPublicationAdapterConfig {
    /// Builds the one production Publication adapter set from process-owned
    /// configuration.
    ///
    /// Requester strings must use canonical `usr_`, `svc_`, or `sys_` IDs.
    ///
    /// # Errors
    ///
    /// Rejects malformed tenant, policy, target, identity, or secret-store
    /// configuration before any adapter is installed.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        scope: RepositoryScope,
        repository: impl Into<String>,
        credential_reference_id: CredentialReferenceId,
        api_base_url: impl Into<String>,
        secret_directory: impl AsRef<Path>,
        allowed_requester_ids: Vec<String>,
        allowed_approvers: Vec<UserId>,
        max_approval_age_millis: u64,
    ) -> Result<Self, LocalPublicationAdapterError> {
        let repository = repository.into();
        let policy_scope = RepositoryPolicyScope::try_new(
            scope.organization_id.clone(),
            scope.workspace_id.clone(),
            scope.project_id.clone(),
            scope.repository_id.clone(),
        )
        .map_err(LocalPublicationAdapterError::new)?;
        let allowed_requesters = allowed_requester_ids
            .into_iter()
            .map(requester_from_id)
            .collect::<Result<Vec<_>, _>>()?;
        let authority = LocalPublicationAuthorityConfig::try_new(
            policy_scope,
            repository.clone(),
            allowed_requesters,
            allowed_approvers,
            max_approval_age_millis,
            "control-plane.publication",
        )
        .map_err(|error| LocalPublicationAdapterError::new(error.to_string()))?;
        let provider = LocalGitHubProviderConfig::new(
            credential_reference_id,
            api_base_url,
            repository,
            Scope::RepositoryScope(scope),
        );
        let secret_directory = secret_directory.as_ref().to_path_buf();
        if secret_directory.as_os_str().is_empty() {
            return Err(LocalPublicationAdapterError::new(
                "Publication secret directory is empty",
            ));
        }
        Ok(Self {
            authority,
            provider,
            secret_directory,
        })
    }
}

/// Startup failure for the production Publication adapters.
#[derive(Debug)]
pub struct LocalPublicationAdapterError {
    message: String,
}

impl LocalPublicationAdapterError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for LocalPublicationAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LocalPublicationAdapterError {}

impl LocalPublicationProviderRegistry {
    /// Opens current Credential-reference metadata and the protected local
    /// secret store used by the GitHub adapter.
    ///
    /// # Errors
    ///
    /// Rejects invalid GitHub configuration or unavailable local stores.
    pub fn open(
        config: LocalGitHubProviderConfig,
        data_directory: impl AsRef<Path>,
        secret_directory: impl AsRef<Path>,
    ) -> Result<Self, PublicationProviderRegistryError> {
        let adapter_config = GitHubAdapterConfig::try_new(
            config.credential_reference_id.clone(),
            config.api_base_url,
        )
        .map_err(|_| PublicationProviderRegistryError::not_configured())?;
        let storage = SqliteStorage::open(data_directory)
            .map_err(|_| PublicationProviderRegistryError::unavailable())?;
        let secrets = LocalSecretStoreAdapter::open(secret_directory)
            .map_err(|_| PublicationProviderRegistryError::unavailable())?;
        Ok(Self {
            credential_reference_id: config.credential_reference_id,
            repository: config.repository,
            scope: config.scope,
            credentials: Box::new(LocalPublicationCredentialSource { storage, secrets }),
            adapter_config,
        })
    }
}

impl PublicationProviderRegistry for LocalPublicationProviderRegistry {
    fn resolve(
        &mut self,
        target: &ApiPublicationTarget,
    ) -> Result<PublicationProviderSession, PublicationProviderRegistryError> {
        if target.provider != PublicationTargetProvider::Github
            || target.repository.0 != self.repository
        {
            return Err(PublicationProviderRegistryError::not_configured());
        }
        let secret = self
            .credentials
            .resolve(&self.scope, &self.credential_reference_id)
            .map_err(|error| map_registry_credential_error(&error))?;
        let session_credentials = SessionGitHubCredentialResolver {
            credential_reference_id: self.credential_reference_id.clone(),
            secret: secret.expose().to_vec(),
        };
        Ok(PublicationProviderSession::new(Box::new(
            GitHubPublicationAdapter::new(self.adapter_config.clone(), session_credentials),
        )))
    }
}

impl ControlPlane {
    /// Installs one immutable Publication authority and provider registry.
    ///
    /// # Errors
    ///
    /// Rejects non-local storage, invalid configuration, unavailable durable
    /// stores, or replacement of either live adapter.
    pub fn install_local_publication_adapters(
        &mut self,
        config: LocalPublicationAdapterConfig,
    ) -> Result<(), LocalPublicationAdapterError> {
        if self.publication_authority.is_some() || self.publication_providers.is_some() {
            return Err(LocalPublicationAdapterError::new(
                "Publication production adapters are already installed",
            ));
        }
        let data_directory = self
            .local_database_path
            .as_deref()
            .and_then(Path::parent)
            .ok_or_else(|| {
                LocalPublicationAdapterError::new(
                    "Publication production adapters require local Control Plane storage",
                )
            })?;
        let authority = LocalPublicationAuthority::open(config.authority)
            .map_err(|error| LocalPublicationAdapterError::new(error.to_string()))?;
        let providers = LocalPublicationProviderRegistry::open(
            config.provider,
            data_directory,
            config.secret_directory,
        )
        .map_err(|error| LocalPublicationAdapterError::new(error.to_string()))?;
        self.publication_authority = Some(Box::new(authority));
        self.publication_providers = Some(Box::new(providers));
        Ok(())
    }

    /// Starts all local production adapters before returning a runnable host.
    ///
    /// # Errors
    ///
    /// Shuts the partially opened host down and returns one startup error when
    /// Delivery or Publication composition fails.
    pub fn start_local_with_production_adapters(
        config: crate::ControlPlaneConfig,
        publisher: Box<dyn EventPublisher>,
        delivery: LocalDeliveryAdapterConfig,
        publication: LocalPublicationAdapterConfig,
    ) -> Result<Self, StartError> {
        let mut control_plane =
            Self::start_local_with_delivery_adapters(config, publisher, delivery)?;
        if let Err(error) = control_plane.install_local_publication_adapters(publication) {
            let cleanup = control_plane
                .shutdown()
                .err()
                .map_or_else(String::new, |source| {
                    format!("; cleanup also failed: {source}")
                });
            return Err(StartError::new(format!(
                "failed to install production Publication adapters: {error}{cleanup}"
            )));
        }
        Ok(control_plane)
    }

    /// Publishes from one generated command after resolving all current
    /// Delivery, policy, approval, evidence, and provider facts internally.
    ///
    /// Exact receipt replay returns before consulting any authority, Artifact,
    /// Credential, or provider adapter.
    ///
    /// # Errors
    ///
    /// Rejects changed request identity, stale/foreign current facts, denied
    /// policy, missing/revoked Credential references, unavailable providers,
    /// and durable storage failures.
    pub fn publication_publish(
        &mut self,
        command: &PublicationPublishCommand,
    ) -> Result<PublicationPublishCompletedResponse, PublicationCommandError> {
        validate_publish_command(command)?;
        let (identity, digest) = publish_receipt(command)?;
        if let Some(publication) = PublicationReadLedger::new(
            self.storage_ref()
                .map_err(|error| PublicationCommandError::Publication(error.into()))?,
        )
        .replay(&identity, &digest)?
        {
            return publish_response(command, &publication);
        }

        let read =
            load_current_publication_read(self, &command.scope, &command.payload.delivery_id)
                .map_err(|_| PublicationAuthorityError::trusted_facts_unavailable())?;
        let candidate = read
            .candidate()
            .cloned()
            .ok_or_else(PublicationAuthorityError::trusted_facts_unavailable)?;
        let user_id = match &command.actor {
            winwincode_api::generated::Actor::UserActor(actor) => actor.id.clone(),
            winwincode_api::generated::Actor::ServiceAccountActor(_)
            | winwincode_api::generated::Actor::SystemActor(_) => {
                return Err(PublicationAuthorityError::trusted_facts_unavailable().into());
            }
        };
        let prepared = self
            .prepare_publication(&command.scope, &candidate, &user_id)
            .map_err(|_| PublicationAuthorityError::trusted_facts_unavailable())?;
        let observed_at_millis = publication_now_millis()?;
        let policy_scope = RepositoryPolicyScope::try_new(
            command.scope.organization_id.clone(),
            command.scope.workspace_id.clone(),
            command.scope.project_id.clone(),
            command.scope.repository_id.clone(),
        )
        .map_err(PublicationCommandError::InvalidInput)?;
        let attribution = PublicationEnterpriseAttribution::try_new(
            &policy_scope,
            command.payload.delivery_id.clone(),
            candidate.producer_product_session_id().clone(),
            user_id,
        )
        .map_err(|_| PublicationAuthorityError::trusted_facts_unavailable())?;

        self.commit_resolved_publication_publish(
            command,
            prepared.authorization(),
            &attribution,
            observed_at_millis,
        )
    }

    fn commit_resolved_publication_publish(
        &mut self,
        command: &PublicationPublishCommand,
        authorization: &PublicationAuthorization,
        attribution: &PublicationEnterpriseAttribution,
        observed_at_millis: u64,
    ) -> Result<PublicationPublishCompletedResponse, PublicationCommandError> {
        let mut authority = self
            .publication_authority
            .take()
            .ok_or_else(PublicationAuthorityError::trusted_facts_unavailable)?;
        let facts = authority.resolve(PublicationAuthorityRequest::new(
            command,
            authorization,
            attribution,
            observed_at_millis,
        ));
        self.publication_authority = Some(authority);
        let facts = facts?;

        let quota_directory = self
            .local_database_path()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                PublicationCommandError::Publication(
                    winwincode_publication::PublicationError::from(
                        winwincode_storage::StorageError::adapter(
                            "local Publication quota database is unavailable",
                        ),
                    ),
                )
            })?;
        let quota_storage = SqliteStorage::open(&quota_directory).map_err(|error| {
            PublicationCommandError::Publication(winwincode_publication::PublicationError::from(
                error,
            ))
        })?;
        let mut quota = crate::DurableEnterpriseQuotaAdmission::new(quota_storage);
        let requested_at = publication_quota_requested_at(authorization.approved_at_millis())
            .map_err(|error| {
                PublicationCommandError::Publication(
                    winwincode_publication::PublicationError::from(error),
                )
            })?;

        enforce_publication_policy(&quota_directory, command, authorization, attribution)?;

        let mut providers = self
            .publication_providers
            .take()
            .ok_or_else(PublicationProviderRegistryError::not_configured)?;
        let session = providers.resolve(&command.payload.target);
        self.publication_providers = Some(providers);
        let mut session = session?;

        let publication = {
            let mut guarded = PublicationEnterpriseQuotaSaga::new(
                &mut quota,
                session.port(),
                facts.attribution(),
                &command.payload.publication_id,
                requested_at,
            );
            self.commit_publication_publish(
                command,
                facts.authorization(),
                facts.attribution(),
                facts.policy(),
                facts.evidence(),
                facts.origin(),
                &mut guarded,
            )?
        };
        quota.close().map_err(|error| {
            PublicationCommandError::Publication(winwincode_publication::PublicationError::from(
                error,
            ))
        })?;
        let mut usage_storage = SqliteStorage::open(&quota_directory).map_err(|error| {
            PublicationCommandError::Publication(winwincode_publication::PublicationError::from(
                error,
            ))
        })?;
        let reconciliation = PublicationEnterpriseUsageReconciler::new(&mut usage_storage)
            .reconcile_exact_publication(&command.payload.publication_id)
            .map_err(|_| {
                PublicationCommandError::Publication(
                    winwincode_publication::PublicationError::from(
                        winwincode_storage::StorageError::adapter(
                            "Publication enterprise Usage reconciliation failed",
                        ),
                    ),
                )
            });
        let close = Box::new(usage_storage).close();
        reconciliation?;
        close.map_err(|error| {
            PublicationCommandError::Publication(winwincode_publication::PublicationError::from(
                error,
            ))
        })?;
        publish_response(command, &publication)
    }
}

fn enforce_publication_policy(
    quota_directory: &Path,
    command: &PublicationPublishCommand,
    authorization: &PublicationAuthorization,
    attribution: &PublicationEnterpriseAttribution,
) -> Result<(), PublicationCommandError> {
    let mut enterprise_policy =
        crate::DurablePublicationPolicyEnforcement::open(quota_directory)
            .map_err(|_| PublicationCommandError::EnterprisePolicyUnavailable)?;
    let result = enterprise_policy.enforce(command, authorization, attribution);
    enterprise_policy
        .close()
        .map_err(|_| PublicationCommandError::EnterprisePolicyUnavailable)?;
    result.map(|_| ()).map_err(|error| match error.kind() {
        crate::PublicationEnterprisePolicyErrorKind::Rejected => {
            PublicationCommandError::EnterprisePolicyDenied
        }
        crate::PublicationEnterprisePolicyErrorKind::Unavailable => {
            PublicationCommandError::EnterprisePolicyUnavailable
        }
    })
}

fn validate_publish_command(
    command: &PublicationPublishCommand,
) -> Result<(), PublicationCommandError> {
    if command.schema_version != SchemaVersion::WinwincodeV1
        || command.expected_revision != Revision(0)
        || command.payload.target.provider != PublicationTargetProvider::Github
    {
        return Err(PublicationCommandError::InvalidInput(
            "publication.publish command is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn publish_receipt(
    command: &PublicationPublishCommand,
) -> Result<
    (
        winwincode_storage::ReceiptIdentity,
        winwincode_domain::Sha256Digest,
    ),
    PublicationCommandError,
> {
    let envelope = CommandEnvelope {
        actor: command.actor.clone(),
        command: CommandName::PublicationPublish,
        expected_revision: command.expected_revision.clone(),
        payload: serde_json::to_value(&command.payload).map_err(|_| {
            PublicationCommandError::InvalidInput(
                "publication.publish payload cannot be encoded".to_owned(),
            )
        })?,
        request_id: command.request_id.clone(),
        schema_version: command.schema_version.clone(),
        scope: Scope::RepositoryScope(command.scope.clone()),
    };
    command_receipt(&envelope).map_err(|error| PublicationCommandError::Publication(error.into()))
}

fn publish_response(
    command: &PublicationPublishCommand,
    publication: &winwincode_publication::Publication,
) -> Result<PublicationPublishCompletedResponse, PublicationCommandError> {
    let result = publication_projection(publication)?;
    checked_response(PublicationPublishCompletedResponse {
        command: PublicationPublishCompletedResponseCommand::PublicationPublish,
        current_revision: result.revision.clone(),
        outcome: PublicationPublishCompletedResponseOutcome::Completed,
        previous_revision: Revision(0),
        request_id: command.request_id.clone(),
        result,
        schema_version: SchemaVersion::WinwincodeV1,
    })
}

fn publication_now_millis() -> Result<u64, PublicationAuthorityError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| PublicationAuthorityError::trusted_facts_unavailable())?
        .as_millis();
    let millis = u64::try_from(millis)
        .map_err(|_| PublicationAuthorityError::trusted_facts_unavailable())?;
    if millis > MAX_SAFE_INTEGER {
        return Err(PublicationAuthorityError::trusted_facts_unavailable());
    }
    Ok(millis)
}

fn requester_from_id(id: String) -> Result<PublicationRequester, LocalPublicationAdapterError> {
    if id.starts_with("usr_") {
        return Ok(PublicationRequester::User(UserId(id)));
    }
    if id.starts_with("svc_") {
        return Ok(PublicationRequester::ServiceAccount(ServiceAccountId(id)));
    }
    if id.starts_with("sys_") {
        return Ok(PublicationRequester::System(SystemActorId(id)));
    }
    Err(LocalPublicationAdapterError::new(
        "Publication requester identity is invalid",
    ))
}

fn map_credential_error(error: CredentialSecretResolutionError) -> CredentialResolutionError {
    match error {
        CredentialSecretResolutionError::Reference(error) => match error.kind() {
            CredentialReferenceErrorKind::ScopeDenied | CredentialReferenceErrorKind::Revoked => {
                CredentialResolutionError::permission_denied()
            }
            CredentialReferenceErrorKind::NotFound | CredentialReferenceErrorKind::WrongState => {
                CredentialResolutionError::not_configured()
            }
            CredentialReferenceErrorKind::InvalidRequest
            | CredentialReferenceErrorKind::RevisionConflict
            | CredentialReferenceErrorKind::RequestConflict
            | CredentialReferenceErrorKind::CursorInvalid
            | CredentialReferenceErrorKind::CredentialLeak
            | CredentialReferenceErrorKind::Storage => CredentialResolutionError::unavailable(),
        },
        CredentialSecretResolutionError::SecretStore(error) => match error.kind() {
            SecretStoreErrorKind::Missing => CredentialResolutionError::not_configured(),
            SecretStoreErrorKind::VersionConflict
            | SecretStoreErrorKind::Unavailable
            | SecretStoreErrorKind::Corrupt => CredentialResolutionError::unavailable(),
        },
    }
}

fn map_registry_credential_error(
    error: &CredentialResolutionError,
) -> PublicationProviderRegistryError {
    match error.code() {
        "credential-not-configured" => PublicationProviderRegistryError::not_configured(),
        "credential-resolution-denied" => PublicationProviderRegistryError::permission_denied(),
        _ => PublicationProviderRegistryError::unavailable(),
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        thread,
    };

    use sha2::{Digest, Sha256};
    use winwincode_api::generated::{
        Actor, PublicationPublishCommandCommand, PublicationPublishPayload,
    };
    use winwincode_domain::{
        EnterprisePolicyId, Instant, RepositoryScopeKind, RequestId, UserActor, UserActorKind,
    };
    use winwincode_publication::{
        PublicationOperation, PublicationPortError, PublicationPortMutation,
        PublicationPortObservation, PublicationRequester,
        test_support::{
            CurrentPublicationFixture, current_publication_fixture, current_publication_operations,
        },
    };
    use winwincode_storage::{
        EnterprisePolicyActor, EnterprisePolicyChildOverrideMode, EnterprisePolicyDefinition,
        EnterprisePolicyEffect, EnterprisePolicyInheritanceMode, EnterprisePolicyKind,
        EnterprisePolicyMode, EnterprisePolicyScope, EnterprisePolicyState,
        EnterprisePolicyVersionSource, EnterprisePolicyWrite,
    };

    use super::*;

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn local_authority_accepts_only_the_exact_sealed_scope_candidate_and_target() {
        let fixture = current_publication_fixture();
        let requester = UserId("usr_00000000000000000000000002".to_owned());
        let scope = repository_scope();
        let config = LocalPublicationAuthorityConfig::try_new(
            RepositoryPolicyScope::try_new(
                scope.organization_id.clone(),
                scope.workspace_id.clone(),
                scope.project_id.clone(),
                scope.repository_id.clone(),
            )
            .expect("fixture policy scope"),
            "example/widget",
            vec![PublicationRequester::User(requester.clone())],
            vec![UserId(fixture.authorization().approved_by().to_owned())],
            10_000,
            "publication-production-test",
        )
        .expect("fixture authority config");
        let mut authority = LocalPublicationAuthority::open(config).expect("local authority");
        let command = publish_command(&fixture, scope, requester);
        let observed_at = fixture.authorization().approved_at_millis() + 1;

        let facts = authority
            .resolve(PublicationAuthorityRequest::new(
                &command,
                fixture.authorization(),
                fixture.attribution(),
                observed_at,
            ))
            .expect("exact sealed authority");
        assert_eq!(facts.authorization(), fixture.authorization());
        assert_eq!(facts.evidence().observed_at_millis(), observed_at);

        let mut foreign = command;
        foreign.payload.target.head_branch = "foreign/head".to_owned();
        let Err(error) = authority.resolve(PublicationAuthorityRequest::new(
            &foreign,
            fixture.authorization(),
            fixture.attribution(),
            observed_at,
        )) else {
            panic!("foreign target must fail closed");
        };
        assert_eq!(
            error.kind(),
            PublicationAuthorityErrorKind::TrustedFactsUnavailable
        );
    }

    #[test]
    fn provider_session_reads_secret_once_and_keeps_that_snapshot_across_rotation() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture GitHub server");
        let address = listener.local_addr().expect("fixture GitHub address");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_request = Arc::clone(&captured);
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept GitHub request");
                let mut bytes = vec![0_u8; 8_192];
                let count = stream.read(&mut bytes).expect("read GitHub request");
                captured_request
                    .lock()
                    .expect("captured request lock")
                    .push(String::from_utf8_lossy(&bytes[..count]).into_owned());
                stream
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\n{}")
                    .expect("write GitHub response");
            }
        });

        let reference = CredentialReferenceId("crd_00000000000000000000000001".to_owned());
        let current_secret = Arc::new(Mutex::new(b"INITIAL_GITHUB_SECRET".to_vec()));
        let resolution_count = Arc::new(AtomicU64::new(0));
        let source = RotatingCredentialSource {
            current_secret: Arc::clone(&current_secret),
            resolution_count: Arc::clone(&resolution_count),
        };
        let mut registry = LocalPublicationProviderRegistry {
            credential_reference_id: reference.clone(),
            repository: "example/widget".to_owned(),
            scope: Scope::RepositoryScope(repository_scope()),
            credentials: Box::new(source),
            adapter_config: GitHubAdapterConfig::try_new(reference, format!("http://{address}"))
                .expect("loopback GitHub config"),
        };
        let target = ApiPublicationTarget {
            provider: PublicationTargetProvider::Github,
            repository: winwincode_domain::GitHubRepositorySlug("example/widget".to_owned()),
            base_branch: "main".to_owned(),
            head_repository: winwincode_domain::GitHubRepositorySlug("example/widget".to_owned()),
            head_branch: "winwincode/delivery".to_owned(),
        };
        let mut session = registry.resolve(&target).expect("one provider session");
        *current_secret.lock().expect("rotate source secret") = b"ROTATED_GITHUB_SECRET".to_vec();

        for operation in current_publication_operations().into_iter().take(2) {
            session
                .port()
                .lookup(&operation)
                .expect("lookup through frozen provider session");
        }
        server.join().expect("fixture GitHub server");

        assert_eq!(resolution_count.load(Ordering::Relaxed), 1);
        let requests = captured.lock().expect("captured request");
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| {
            let request = request.to_ascii_lowercase();
            request.contains("authorization: bearer initial_github_secret")
                && !request.contains("rotated_github_secret")
        }));
    }

    #[test]
    fn missing_and_revoked_provider_fail_before_intent_outbox_or_port_effect() {
        assert_provider_failure_has_zero_writes(
            PublicationProviderRegistryErrorKind::NotConfigured,
        );
        assert_provider_failure_has_zero_writes(
            PublicationProviderRegistryErrorKind::PermissionDenied,
        );
    }

    #[test]
    fn local_production_publish_opens_the_quota_port_and_injected_host_fails_closed() {
        let fixture = current_publication_fixture();
        let requester = UserId("usr_00000000000000000000000002".to_owned());
        let scope = repository_scope();
        let command = publish_command(&fixture, scope, requester.clone());
        let root = std::env::temp_dir().join(format!(
            "winwincode-publication-quota-production-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));

        let local_calls = Arc::new(AtomicU64::new(0));
        let mut local = ControlPlane::start_local(
            crate::ControlPlaneConfig::local(&root),
            Box::new(SilentPublisher),
        )
        .expect("start local Control Plane");
        local.publication_authority = Some(Box::new(
            fixture_authority(&fixture, &requester).expect("local authority"),
        ));
        local.publication_providers = Some(Box::new(CountingProviderRegistry {
            calls: Arc::clone(&local_calls),
        }));
        local
            .commit_resolved_publication_publish(
                &command,
                fixture.authorization(),
                fixture.attribution(),
                fixture.authorization().approved_at_millis() + 1,
            )
            .expect("local host opens the quota connection before publication commit");
        assert_eq!(local_calls.load(Ordering::Relaxed), 1);
        local.shutdown().expect("shutdown local Control Plane");

        let injected_calls = Arc::new(AtomicU64::new(0));
        let storage = SqliteStorage::open(&root).expect("open injected storage");
        let mut injected = ControlPlane::start(Box::new(storage), Box::new(SilentPublisher))
            .expect("start injected Control Plane");
        injected.publication_authority = Some(Box::new(
            fixture_authority(&fixture, &requester).expect("injected authority"),
        ));
        injected.publication_providers = Some(Box::new(CountingProviderRegistry {
            calls: Arc::clone(&injected_calls),
        }));
        let error = injected
            .commit_resolved_publication_publish(
                &command,
                fixture.authorization(),
                fixture.attribution(),
                fixture.authorization().approved_at_millis() + 1,
            )
            .expect_err("injected host has no local quota database identity");
        assert_eq!(
            error.public_code(),
            winwincode_api::generated::ErrorCode::ServiceUnavailable
        );
        assert_eq!(injected_calls.load(Ordering::Relaxed), 0);
        injected
            .shutdown()
            .expect("shutdown injected Control Plane");
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn enterprise_publication_policy_denies_before_provider_resolution_and_replays_one_audit() {
        let fixture = current_publication_fixture();
        let requester = UserId("usr_00000000000000000000000002".to_owned());
        let scope = repository_scope();
        let command = publish_command(&fixture, scope.clone(), requester.clone());
        let root = std::env::temp_dir().join(format!(
            "winwincode-publication-enterprise-policy-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let calls = Arc::new(AtomicU64::new(0));
        let mut control_plane = ControlPlane::start_local(
            crate::ControlPlaneConfig::local(&root),
            Box::new(SilentPublisher),
        )
        .expect("start local Control Plane");
        control_plane.publication_authority = Some(Box::new(
            fixture_authority(&fixture, &requester).expect("local authority"),
        ));
        control_plane.publication_providers = Some(Box::new(CountingProviderRegistry {
            calls: Arc::clone(&calls),
        }));
        seed_publication_deny_policy(&root, &scope, &requester);

        for _ in 0..2 {
            let error = control_plane
                .commit_resolved_publication_publish(
                    &command,
                    fixture.authorization(),
                    fixture.attribution(),
                    fixture.authorization().approved_at_millis() + 1,
                )
                .expect_err("enterprise Publication Policy must deny");
            assert_eq!(
                error.public_code(),
                winwincode_api::generated::ErrorCode::PermissionDenied
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let mut audit_storage = SqliteStorage::open(&root).expect("open Policy audit storage");
        assert_eq!(
            audit_storage
                .enterprise_policy_evaluation_ledger()
                .expect("open Policy audit")
                .scan_audit(None, 10)
                .expect("scan Policy audit")
                .entries
                .len(),
            1
        );
        drop(audit_storage);
        control_plane.shutdown().expect("shutdown Control Plane");
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    fn seed_publication_deny_policy(root: &Path, scope: &RepositoryScope, requester: &UserId) {
        let definition = EnterprisePolicyDefinition {
            default_effect: EnterprisePolicyEffect::Deny,
            child_override_mode: EnterprisePolicyChildOverrideMode::TightenOnly,
            rules: Vec::new(),
        };
        let canonical = serde_json::to_value(&definition).expect("Policy value fixture");
        let definition_sha256 = winwincode_domain::Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&canonical).expect("serialize Policy definition"))
        ));
        SqliteStorage::open(root)
            .expect("open Policy storage")
            .enterprise_policy_ledger()
            .expect("open Policy ledger")
            .write(&EnterprisePolicyWrite {
                policy_id: EnterprisePolicyId("pol_00000000000000000000000090".to_owned()),
                policy_kind: EnterprisePolicyKind::Publication,
                scope: EnterprisePolicyScope::Organization {
                    organization_id: scope.organization_id.clone(),
                },
                mode: EnterprisePolicyMode::Enforce,
                state: EnterprisePolicyState::Active,
                definition_sha256,
                definition,
                effective_at: Instant("1970-01-01T00:00:00.000Z".to_owned()),
                inheritance_mode: EnterprisePolicyInheritanceMode::Tighten,
                base_version: None,
                expected_revision: 0,
                source: EnterprisePolicyVersionSource {
                    actor: EnterprisePolicyActor::User {
                        id: requester.clone(),
                    },
                    request_id: RequestId("req_00000000000000000000000090".to_owned()),
                },
                updated_at: Instant("2020-01-01T00:00:00.000Z".to_owned()),
            })
            .expect("write Publication deny Policy");
    }

    fn assert_provider_failure_has_zero_writes(kind: PublicationProviderRegistryErrorKind) {
        let fixture = current_publication_fixture();
        let requester = UserId("usr_00000000000000000000000002".to_owned());
        let scope = repository_scope();
        let config = LocalPublicationAuthorityConfig::try_new(
            RepositoryPolicyScope::try_new(
                scope.organization_id.clone(),
                scope.workspace_id.clone(),
                scope.project_id.clone(),
                scope.repository_id.clone(),
            )
            .expect("fixture policy scope"),
            "example/widget",
            vec![PublicationRequester::User(requester.clone())],
            vec![UserId(fixture.authorization().approved_by().to_owned())],
            10_000,
            "publication-production-test",
        )
        .expect("fixture authority config");
        let calls = Arc::new(AtomicU64::new(0));
        let root = std::env::temp_dir().join(format!(
            "winwincode-publication-zero-write-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut control_plane = ControlPlane::start_local(
            crate::ControlPlaneConfig::local(&root),
            Box::new(SilentPublisher),
        )
        .expect("fixture Control Plane");
        control_plane.publication_authority = Some(Box::new(
            LocalPublicationAuthority::open(config).expect("fixture authority"),
        ));
        control_plane.publication_providers = Some(Box::new(FailingProviderRegistry {
            kind,
            calls: Arc::clone(&calls),
        }));
        let command = publish_command(&fixture, scope, requester);
        let error = control_plane
            .commit_resolved_publication_publish(
                &command,
                fixture.authorization(),
                fixture.attribution(),
                fixture.authorization().approved_at_millis() + 1,
            )
            .expect_err("provider failure must precede the Publication commit");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            error.public_code(),
            match kind {
                PublicationProviderRegistryErrorKind::PermissionDenied => {
                    winwincode_api::generated::ErrorCode::PermissionDenied
                }
                PublicationProviderRegistryErrorKind::NotConfigured
                | PublicationProviderRegistryErrorKind::Unavailable => {
                    winwincode_api::generated::ErrorCode::ServiceUnavailable
                }
            }
        );
        assert!(
            control_plane
                .load_state(&format!("publication:{}", command.payload.publication_id.0))
                .expect("Publication state read")
                .is_none()
        );
        assert!(
            control_plane
                .storage_ref()
                .expect("fixture storage")
                .pending_events()
                .expect("pending outbox read")
                .is_empty()
        );
        control_plane.shutdown().expect("fixture shutdown");
        std::fs::remove_dir_all(root).expect("fixture cleanup");
    }

    struct SilentPublisher;

    impl EventPublisher for SilentPublisher {
        fn publish(
            &mut self,
            _event: &winwincode_storage::OutboxEvent,
        ) -> Result<(), crate::EventPublishError> {
            Ok(())
        }
    }

    struct FailingProviderRegistry {
        kind: PublicationProviderRegistryErrorKind,
        calls: Arc<AtomicU64>,
    }

    struct CountingProviderRegistry {
        calls: Arc<AtomicU64>,
    }

    impl PublicationProviderRegistry for CountingProviderRegistry {
        fn resolve(
            &mut self,
            _target: &ApiPublicationTarget,
        ) -> Result<PublicationProviderSession, PublicationProviderRegistryError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(PublicationProviderSession::new(Box::new(
                NoopPublicationPort,
            )))
        }
    }

    struct NoopPublicationPort;

    impl PublicationPort for NoopPublicationPort {
        fn lookup(
            &mut self,
            operation: &PublicationOperation,
        ) -> Result<PublicationPortObservation, PublicationPortError> {
            Ok(PublicationPortObservation::absent(operation))
        }

        fn apply(
            &mut self,
            operation: &PublicationOperation,
        ) -> Result<PublicationPortMutation, PublicationPortError> {
            Ok(PublicationPortMutation::unknown(operation, "not-used"))
        }
    }

    impl PublicationProviderRegistry for FailingProviderRegistry {
        fn resolve(
            &mut self,
            _target: &ApiPublicationTarget,
        ) -> Result<PublicationProviderSession, PublicationProviderRegistryError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err(match self.kind {
                PublicationProviderRegistryErrorKind::NotConfigured => {
                    PublicationProviderRegistryError::not_configured()
                }
                PublicationProviderRegistryErrorKind::PermissionDenied => {
                    PublicationProviderRegistryError::permission_denied()
                }
                PublicationProviderRegistryErrorKind::Unavailable => {
                    PublicationProviderRegistryError::unavailable()
                }
            })
        }
    }

    struct RotatingCredentialSource {
        current_secret: Arc<Mutex<Vec<u8>>>,
        resolution_count: Arc<AtomicU64>,
    }

    impl PublicationCredentialSource for RotatingCredentialSource {
        fn resolve(
            &mut self,
            _scope: &Scope,
            _credential_reference_id: &CredentialReferenceId,
        ) -> Result<crate::ResolvedSecret, CredentialResolutionError> {
            self.resolution_count.fetch_add(1, Ordering::Relaxed);
            crate::ResolvedSecret::from_bytes(
                self.current_secret
                    .lock()
                    .expect("source secret lock")
                    .clone(),
            )
            .map_err(|_| CredentialResolutionError::unavailable())
        }
    }

    fn repository_scope() -> RepositoryScope {
        RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: winwincode_domain::OrganizationId(
                "org_00000000000000000000000001".to_owned(),
            ),
            workspace_id: winwincode_domain::WorkspaceId(
                "wsp_00000000000000000000000001".to_owned(),
            ),
            project_id: winwincode_domain::ProjectId("prj_00000000000000000000000001".to_owned()),
            repository_id: winwincode_domain::RepositoryId(
                "rep_00000000000000000000000001".to_owned(),
            ),
        }
    }

    fn fixture_authority(
        fixture: &CurrentPublicationFixture,
        requester: &UserId,
    ) -> Result<LocalPublicationAuthority, PublicationAuthorityError> {
        let scope = repository_scope();
        LocalPublicationAuthority::open(LocalPublicationAuthorityConfig::try_new(
            RepositoryPolicyScope::try_new(
                scope.organization_id,
                scope.workspace_id,
                scope.project_id,
                scope.repository_id,
            )
            .map_err(|_| PublicationAuthorityError::trusted_facts_unavailable())?,
            "example/widget",
            vec![PublicationRequester::User(requester.clone())],
            vec![UserId(fixture.authorization().approved_by().to_owned())],
            10_000,
            "publication-production-test",
        )?)
    }

    fn publish_command(
        fixture: &CurrentPublicationFixture,
        scope: RepositoryScope,
        requester: UserId,
    ) -> PublicationPublishCommand {
        let authorization = fixture.authorization();
        PublicationPublishCommand {
            actor: Actor::UserActor(UserActor {
                id: requester,
                kind: UserActorKind::User,
            }),
            command: PublicationPublishCommandCommand::PublicationPublish,
            expected_revision: Revision(0),
            payload: PublicationPublishPayload {
                publication_id: fixture.publication_id().clone(),
                delivery_id: authorization.binding().delivery_id().clone(),
                candidate_digest: authorization.candidate_digest().clone(),
                target: ApiPublicationTarget {
                    provider: PublicationTargetProvider::Github,
                    repository: winwincode_domain::GitHubRepositorySlug(
                        authorization.target().repository().to_owned(),
                    ),
                    base_branch: authorization.target().base_branch().to_owned(),
                    head_repository: winwincode_domain::GitHubRepositorySlug(
                        authorization.target().head_repository().to_owned(),
                    ),
                    head_branch: authorization.target().head_branch().to_owned(),
                },
            },
            request_id: RequestId("req_00000000000000000000000999".to_owned()),
            schema_version: SchemaVersion::WinwincodeV1,
            scope,
        }
    }
}
