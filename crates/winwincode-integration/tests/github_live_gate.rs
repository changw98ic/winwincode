// SPDX-License-Identifier: Apache-2.0

#![allow(
    clippy::too_many_lines,
    reason = "the live gate keeps its secret boundary, canonical path assembly, and evidence checks together"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::fs::{self, DirBuilder};
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use rcgen::{CertifiedKey, KeyPair, PKCS_RSA_SHA256, generate_simple_self_signed};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use winwincode_api::generated::{
    AcceptanceCriterionInput, Actor, DeliveryCreateCommand, DeliveryCreateCommandCommand,
    DeliveryCreatePayload, DeliverySpecInput, PublicationTarget as ApiPublicationTarget,
    PublicationTargetProvider, RepositoryScope, RepositoryScopeKind, SystemActor, SystemActorKind,
};
use winwincode_audit::AuditScope;
use winwincode_domain::{
    AttentionItemId, CredentialReferenceId, DeliveryId, EnterpriseIntegrationId,
    GitHubRepositorySlug, OrganizationId, ProductSessionId, ProjectId, PublicationId, RepositoryId,
    RequestId, Revision, SchemaVersion, Sha256Digest, SystemActorId, UserId, WorkspaceId,
};
use winwincode_integration::{
    ConnectorCallError, ConnectorCallErrorKind, ConnectorPort, ConnectorProtocol,
    ConnectorRegistration, GITHUB_CONNECTOR_PROTOCOL, GitHubAppId, GitHubClock,
    GitHubConnectorConfig, GitHubCredentialError, GitHubCredentialErrorKind, GitHubCredentialPort,
    GitHubEnterpriseConnector, GitHubEventMapperPort, GitHubInboundEvent, GitHubInstallationId,
    GitHubInstallationPermissions, GitHubInstallationToken, GitHubPermission, GitHubTlsRoots,
    GitHubWebhookHeaders, GitHubWebhookRequestFactory, GitHubWebhookSecret, GitHubWebhookVerifier,
    InboundStatus, IntegrationFramework, IntegrationLeaseId, IntegrationOperationKey,
    IntegrationStorage, NormalizedInboundEvent, OutboundAttemptResult, OutboundOperationState,
    OutboundRequest, RetryPolicy,
};
use winwincode_publication::{
    CredentialResolutionError, GitHubCredential, GitHubCredentialResolver, PolicyPermission,
    PublicationAuthorization, PublicationCommandContext, PublicationEnterpriseAttribution,
    PublicationFactBinding, PublicationLedger, PublicationPolicyAudit, PublicationPolicyAuditError,
    PublicationPolicyContext, PublicationPolicyDecision, PublicationPolicyEvidence,
    PublicationPolicyOrigin, PublicationPublishCommand, PublicationRequester,
    PublicationResourceKind, PublicationSourceIssue, PublicationState, PublicationTarget,
    RepositoryPolicyScope, RepositoryPublicationPolicy,
};
use winwincode_storage::{
    ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
};

const LIVE_GATE_ENV: &str = "WINWINCODE_GITHUB_LIVE_GATE";
const CONFIG_FILE_ENV: &str = "WINWINCODE_GITHUB_LIVE_CONFIG_FILE";
const PRIVATE_KEY_FILE_ENV: &str = "WINWINCODE_GITHUB_LIVE_APP_PRIVATE_KEY_FILE";
const WEBHOOK_SECRET_FILE_ENV: &str = "WINWINCODE_GITHUB_LIVE_WEBHOOK_SECRET_FILE";
const WEBHOOK_PAYLOAD_FILE_ENV: &str = "WINWINCODE_GITHUB_LIVE_WEBHOOK_PAYLOAD_FILE";
const STATE_DIRECTORY_ENV: &str = "WINWINCODE_GITHUB_LIVE_STATE_DIRECTORY";
const LEGACY_TOKEN_ENV: &str = "WINWINCODE_GITHUB_LIVE_TOKEN";

const CONFIG_SCHEMA_VERSION: u8 = 1;
const MAX_CONFIG_BYTES: u64 = 128 * 1_024;
const MAX_PRIVATE_KEY_BYTES: u64 = 1024 * 1024;
const MAX_WEBHOOK_SECRET_BYTES: u64 = 4_096;
const MAX_WEBHOOK_PAYLOAD_BYTES: u64 = 2 * 1_024 * 1_024;
const MAX_GITHUB_RESPONSE_BYTES: u64 = 2 * 1_024 * 1_024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const INSTALLATION_TOKEN_REFRESH_MARGIN_MILLIS: u64 = 60_000;
const INSTALLATION_TOKEN_MAX_LIFETIME_MILLIS: u64 = 3_700_000;
const OWNER_ONLY_DIRECTORY_MODE: u32 = 0o700;

const REQUIRED_PERMISSION_PAIRS: [(&str, &str); 6] = [
    ("checks", "write"),
    ("contents", "write"),
    ("issues", "write"),
    ("metadata", "read"),
    ("pull_requests", "write"),
    ("statuses", "write"),
];
const REQUIRED_EVENTS: [&str; 4] = ["check_run", "issues", "pull_request", "pull_request_review"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GateErrorCode {
    MissingConfiguration,
    InvalidConfiguration,
    UnsafeCredentialFile,
    InvalidCredential,
    GitHubAuthentication,
    GitHubInstallationScope,
    GitHubResponse,
    CanonicalPath,
    SecretLeak,
    DurableState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GateError {
    code: GateErrorCode,
}

impl GateError {
    const fn new(code: GateErrorCode) -> Self {
        Self { code }
    }

    const fn code(self) -> GateErrorCode {
        self.code
    }
}

impl fmt::Display for GateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.code {
            GateErrorCode::MissingConfiguration => "GitHub live-gate configuration is missing",
            GateErrorCode::InvalidConfiguration => "GitHub live-gate configuration is invalid",
            GateErrorCode::UnsafeCredentialFile => {
                "GitHub live-gate credential file is not owner-only"
            }
            GateErrorCode::InvalidCredential => "GitHub live-gate credential is invalid",
            GateErrorCode::GitHubAuthentication => "GitHub App authentication failed",
            GateErrorCode::GitHubInstallationScope => {
                "GitHub App installation is outside the required minimal scope"
            }
            GateErrorCode::GitHubResponse => "GitHub live-gate response is invalid",
            GateErrorCode::CanonicalPath => "GitHub live gate did not use the canonical path",
            GateErrorCode::SecretLeak => "GitHub live-gate secret leakage check failed",
            GateErrorCode::DurableState => "GitHub live-gate durable state is invalid",
        })
    }
}

impl std::error::Error for GateError {}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GateEnvironment {
    config_file: PathBuf,
    private_key_file: PathBuf,
    webhook_secret_file: PathBuf,
    webhook_payload_file: PathBuf,
    state_directory: PathBuf,
}

impl GateEnvironment {
    fn from_process() -> Result<Self, GateError> {
        if env::var(LIVE_GATE_ENV).as_deref() != Ok("1") || env::var_os(LEGACY_TOKEN_ENV).is_some()
        {
            return Err(GateError::new(GateErrorCode::MissingConfiguration));
        }
        Ok(Self {
            config_file: required_path(CONFIG_FILE_ENV)?,
            private_key_file: required_path(PRIVATE_KEY_FILE_ENV)?,
            webhook_secret_file: required_path(WEBHOOK_SECRET_FILE_ENV)?,
            webhook_payload_file: required_path(WEBHOOK_PAYLOAD_FILE_ENV)?,
            state_directory: required_path(STATE_DIRECTORY_ENV)?,
        })
    }

    fn validate_paths(&self) -> Result<(), GateError> {
        let state = prepare_state_directory(&self.state_directory)?;
        for path in [
            &self.config_file,
            &self.private_key_file,
            &self.webhook_secret_file,
            &self.webhook_payload_file,
        ] {
            let canonical = fs::canonicalize(path)
                .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?;
            if canonical.starts_with(&state) {
                return Err(GateError::new(GateErrorCode::InvalidConfiguration));
            }
        }
        Ok(())
    }
}

fn required_path(name: &str) -> Result<PathBuf, GateError> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| GateError::new(GateErrorCode::MissingConfiguration))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LiveConfigFile {
    schema_version: u8,
    api_base_url: String,
    integration_id: String,
    credential_reference_id: String,
    app_id: u64,
    installation_id: u64,
    repository: String,
    scope: ScopeConfig,
    webhook: WebhookConfig,
    delivery: DeliveryFactsConfig,
    publication: PublicationConfig,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScopeConfig {
    #[serde(rename = "organizationId")]
    organization: String,
    #[serde(rename = "workspaceId")]
    workspace: String,
    #[serde(rename = "projectId")]
    project: String,
    #[serde(rename = "repositoryId")]
    repository: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WebhookConfig {
    delivery_id: String,
    event_type: String,
    signature_256: String,
    received_at_millis: u64,
    issue_number: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeliveryFactsConfig {
    delivery_id: String,
    request_id: String,
    system_actor_id: String,
    delivery_revision: u64,
    delivery_spec_id: String,
    delivery_spec_revision: u64,
    candidate_ref: String,
    diff_sha256: String,
    verdict_id: String,
    approval_id: String,
    approval_review_set_sha256: String,
    candidate_commit_id: String,
    artifact_id: String,
    artifact_digest: String,
    approved_by: String,
    approved_at_millis: u64,
    product_session_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublicationConfig {
    publication_id: String,
    request_id: String,
    base_branch: String,
    head_repository: String,
    head_branch: String,
    max_approval_age_millis: u64,
}

#[derive(Clone)]
struct ValidatedConfig {
    connector: GitHubConnectorConfig,
    api_base_url: String,
    integration_scope: AuditScope,
    repository_scope: RepositoryScope,
    publication_scope: RepositoryPolicyScope,
    webhook: WebhookConfig,
    webhook_payload: Vec<u8>,
    delivery_command: DeliveryCreateCommand,
    authorization: PublicationAuthorization,
    publication_command: PublicationPublishCommand,
    publication_request_id: RequestId,
    attribution: PublicationEnterpriseAttribution,
    requester: PublicationRequester,
    policy: RepositoryPublicationPolicy,
    max_approval_age_millis: u64,
}

impl LiveConfigFile {
    fn read(environment: &GateEnvironment) -> Result<Self, GateError> {
        environment.validate_paths()?;
        let bytes = read_regular_file(&environment.config_file, MAX_CONFIG_BYTES, false)?;
        reject_embedded_credentials(&bytes)?;
        serde_json::from_slice(&bytes)
            .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))
    }

    fn validate(self, webhook_payload: Vec<u8>) -> Result<ValidatedConfig, GateError> {
        if self.schema_version != CONFIG_SCHEMA_VERSION
            || self.webhook.event_type != "issues"
            || self.webhook.issue_number == 0
            || self.webhook.received_at_millis == 0
            || self.webhook.received_at_millis > MAX_SAFE_INTEGER
            || self.publication.max_approval_age_millis == 0
            || self.publication.max_approval_age_millis > MAX_SAFE_INTEGER
        {
            return Err(GateError::new(GateErrorCode::InvalidConfiguration));
        }
        let repository_scope = RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: OrganizationId(self.scope.organization),
            workspace_id: WorkspaceId(self.scope.workspace),
            project_id: ProjectId(self.scope.project),
            repository_id: RepositoryId(self.scope.repository),
        };
        let integration_scope = AuditScope::repository(
            repository_scope.organization_id.clone(),
            repository_scope.workspace_id.clone(),
            repository_scope.project_id.clone(),
            repository_scope.repository_id.clone(),
        )
        .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?;
        let publication_scope = RepositoryPolicyScope::try_new(
            repository_scope.organization_id.clone(),
            repository_scope.workspace_id.clone(),
            repository_scope.project_id.clone(),
            repository_scope.repository_id.clone(),
        )
        .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?;
        let api_base_url = canonical_api_base_url(&self.api_base_url)?;
        let connector = GitHubConnectorConfig::try_new(
            EnterpriseIntegrationId(self.integration_id),
            CredentialReferenceId(self.credential_reference_id),
            GitHubAppId::try_new(self.app_id)
                .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?,
            GitHubInstallationId::try_new(self.installation_id)
                .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?,
            GitHubRepositorySlug(self.repository.clone()),
            api_base_url.clone(),
            GitHubTlsRoots::WebPki,
        )
        .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?;
        let target = PublicationTarget::try_github(
            self.repository.clone(),
            self.publication.base_branch.clone(),
            self.publication.head_repository.clone(),
            self.publication.head_branch.clone(),
        )
        .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?;
        let target_sha256 = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&target)
                    .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?
            )
        );
        let delivery_id = DeliveryId(self.delivery.delivery_id.clone());
        let binding = PublicationFactBinding::try_new(
            delivery_id.clone(),
            self.delivery.delivery_revision,
            self.delivery.delivery_spec_id,
            self.delivery.delivery_spec_revision,
            self.delivery.candidate_ref,
            self.delivery.diff_sha256,
            self.delivery.verdict_id,
            AttentionItemId(self.delivery.approval_id),
            self.delivery.approval_review_set_sha256,
            target_sha256,
        )
        .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?;
        let authorization = PublicationAuthorization::try_from_current_facts(
            binding,
            PublicationSourceIssue::try_github(self.repository.clone(), self.webhook.issue_number)
                .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?,
            target.clone(),
            self.delivery.candidate_commit_id.clone(),
            self.delivery.artifact_id,
            Sha256Digest(self.delivery.artifact_digest),
            self.delivery.approved_by.clone(),
            self.delivery.approved_at_millis,
            publication_scope.sha256(),
        )
        .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?;
        let publication_command = PublicationPublishCommand::try_new(
            PublicationId(self.publication.publication_id),
            delivery_id.clone(),
            authorization.candidate_digest().clone(),
            target,
        )
        .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?;
        let publication_request_id = RequestId(self.publication.request_id);
        let approved_user = UserId(self.delivery.approved_by);
        let requester = PublicationRequester::User(approved_user.clone());
        let max_approval_age_millis = self.publication.max_approval_age_millis;
        let attribution = PublicationEnterpriseAttribution::try_new(
            &publication_scope,
            delivery_id,
            ProductSessionId(self.delivery.product_session_id),
            approved_user.clone(),
        )
        .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?;
        let policy = RepositoryPublicationPolicy::try_new(
            publication_scope.clone(),
            self.repository.clone(),
            vec![requester.clone()],
            Vec::new(),
            vec![approved_user],
            Vec::new(),
            PolicyPermission::Allow,
            true,
            PolicyPermission::Allow,
            max_approval_age_millis,
        )
        .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?;
        let api_target = ApiPublicationTarget {
            base_branch: self.publication.base_branch,
            head_branch: self.publication.head_branch,
            head_repository: GitHubRepositorySlug(self.publication.head_repository),
            provider: PublicationTargetProvider::Github,
            repository: GitHubRepositorySlug(self.repository.clone()),
        };
        let delivery_command = DeliveryCreateCommand {
            actor: Actor::SystemActor(SystemActor {
                id: SystemActorId(self.delivery.system_actor_id),
                kind: SystemActorKind::System,
            }),
            command: DeliveryCreateCommandCommand::DeliveryCreate,
            expected_revision: Revision(0),
            payload: DeliveryCreatePayload {
                delivery_id: DeliveryId(self.delivery.delivery_id),
                spec: DeliverySpecInput {
                    acceptance_criteria: vec![AcceptanceCriterionInput {
                        id: "github-issue-accepted".to_owned(),
                        required: true,
                        title: "Complete the approved GitHub Issue scope".to_owned(),
                    }],
                    base_revision: self.delivery.candidate_commit_id,
                    goal: format!(
                        "Deliver the approved scope from GitHub Issue {}#{}",
                        self.repository, self.webhook.issue_number
                    ),
                    scope: vec!["approved GitHub Issue scope".to_owned()],
                    out_of_scope: Vec::new(),
                    constraints: vec!["repository verification passes".to_owned()],
                    source_product_session_id: None,
                    publication_target: Some(api_target),
                    repository_id: repository_scope.repository_id.clone(),
                    title: format!("GitHub Issue #{} Delivery", self.webhook.issue_number),
                },
                tasks: Vec::new(),
            },
            request_id: RequestId(self.delivery.request_id),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: repository_scope.clone(),
        };
        let factory = GitHubWebhookRequestFactory::new(connector.clone());
        factory
            .build(
                integration_scope.clone(),
                GitHubWebhookHeaders::try_new(
                    self.webhook.delivery_id.clone(),
                    self.webhook.event_type.clone(),
                    self.webhook.signature_256.as_bytes(),
                )
                .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?,
                webhook_payload.clone(),
                self.webhook.received_at_millis,
            )
            .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?;
        validate_issue_payload(
            &webhook_payload,
            &self.repository,
            self.webhook.issue_number,
            self.installation_id,
        )?;
        Ok(ValidatedConfig {
            connector,
            api_base_url,
            integration_scope,
            repository_scope,
            publication_scope,
            webhook: self.webhook,
            webhook_payload,
            delivery_command,
            authorization,
            publication_command,
            publication_request_id,
            attribution,
            requester,
            policy,
            max_approval_age_millis,
        })
    }
}

fn canonical_api_base_url(value: &str) -> Result<String, GateError> {
    if value.trim() != value
        || !value.starts_with("https://")
        || value.contains(['?', '#'])
        || value
            .strip_prefix("https://")
            .is_none_or(|authority| authority.is_empty() || authority.contains('@'))
    {
        return Err(GateError::new(GateErrorCode::InvalidConfiguration));
    }
    Ok(format!("{}/", value.trim_end_matches('/')))
}

fn reject_embedded_credentials(bytes: &[u8]) -> Result<(), GateError> {
    let lower = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    if [
        "-----begin private key-----",
        "-----begin rsa private key-----",
        "\"token\"",
        "\"secret\"",
        "\"privatekey\"",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return Err(GateError::new(GateErrorCode::InvalidConfiguration));
    }
    Ok(())
}

fn validate_issue_payload(
    payload: &[u8],
    repository: &str,
    issue_number: u64,
    installation_id: u64,
) -> Result<(), GateError> {
    let value: Value = serde_json::from_slice(payload)
        .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?;
    let matches = value
        .get("action")
        .and_then(Value::as_str)
        .is_some_and(|action| matches!(action, "opened" | "edited" | "reopened" | "labeled"))
        && value
            .pointer("/repository/full_name")
            .and_then(Value::as_str)
            == Some(repository)
        && value.pointer("/issue/number").and_then(Value::as_u64) == Some(issue_number)
        && value.pointer("/installation/id").and_then(Value::as_u64) == Some(installation_id);
    if matches {
        Ok(())
    } else {
        Err(GateError::new(GateErrorCode::InvalidConfiguration))
    }
}

fn read_regular_file(path: &Path, maximum: u64, owner_only: bool) -> Result<Vec<u8>, GateError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        GateError::new(if owner_only {
            GateErrorCode::UnsafeCredentialFile
        } else {
            GateErrorCode::InvalidConfiguration
        })
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > maximum
    {
        return Err(GateError::new(if owner_only {
            GateErrorCode::UnsafeCredentialFile
        } else {
            GateErrorCode::InvalidConfiguration
        }));
    }
    if owner_only && metadata.permissions().mode() & 0o077 != 0 {
        return Err(GateError::new(GateErrorCode::UnsafeCredentialFile));
    }
    fs::read(path).map_err(|_| {
        GateError::new(if owner_only {
            GateErrorCode::UnsafeCredentialFile
        } else {
            GateErrorCode::InvalidConfiguration
        })
    })
}

fn prepare_state_directory(path: &Path) -> Result<PathBuf, GateError> {
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(OWNER_ONLY_DIRECTORY_MODE);
    builder
        .create(path)
        .map_err(|_| GateError::new(GateErrorCode::DurableState))?;
    fs::set_permissions(path, fs::Permissions::from_mode(OWNER_ONLY_DIRECTORY_MODE))
        .map_err(|_| GateError::new(GateErrorCode::DurableState))?;
    let metadata =
        fs::symlink_metadata(path).map_err(|_| GateError::new(GateErrorCode::DurableState))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(GateError::new(GateErrorCode::DurableState));
    }
    fs::canonicalize(path).map_err(|_| GateError::new(GateErrorCode::DurableState))
}

struct SecretBytes(Vec<u8>);

impl SecretBytes {
    fn read(path: &Path, maximum: u64) -> Result<Self, GateError> {
        read_regular_file(path, maximum, true).map(Self)
    }

    fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretBytes([REDACTED])")
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Debug, Serialize)]
struct GitHubAppClaims {
    iat: u64,
    exp: u64,
    iss: String,
}

#[derive(Deserialize)]
struct InstallationTokenResponse {
    token: String,
    expires_at: String,
    permissions: BTreeMap<String, String>,
    repositories: Vec<InstallationRepository>,
    repository_selection: String,
}

#[derive(Deserialize)]
struct InstallationRepository {
    full_name: String,
}

struct CachedInstallationToken {
    bytes: Vec<u8>,
    expires_at_millis: u64,
}

impl fmt::Debug for CachedInstallationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CachedInstallationToken")
            .field("bytes", &"[REDACTED]")
            .field("expires_at_millis", &self.expires_at_millis)
            .finish()
    }
}

impl Drop for CachedInstallationToken {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

struct BrokerState {
    credential_reference_id: CredentialReferenceId,
    app_id: GitHubAppId,
    installation_id: GitHubInstallationId,
    repository: GitHubRepositorySlug,
    api_base_url: String,
    private_key: SecretBytes,
    webhook_secret: SecretBytes,
    token: Option<CachedInstallationToken>,
    agent: ureq::Agent,
}

#[derive(Clone)]
struct SharedGitHubCredentialBroker {
    state: Arc<Mutex<BrokerState>>,
}

impl SharedGitHubCredentialBroker {
    fn open(config: &ValidatedConfig, environment: &GateEnvironment) -> Result<Self, GateError> {
        let private_key = SecretBytes::read(&environment.private_key_file, MAX_PRIVATE_KEY_BYTES)?;
        EncodingKey::from_rsa_pem(private_key.expose())
            .map_err(|_| GateError::new(GateErrorCode::InvalidCredential))?;
        let webhook_secret =
            SecretBytes::read(&environment.webhook_secret_file, MAX_WEBHOOK_SECRET_BYTES)?;
        GitHubWebhookSecret::try_new(webhook_secret.expose())
            .map_err(|_| GateError::new(GateErrorCode::InvalidCredential))?;
        let agent = github_agent(&GitHubTlsRoots::WebPki);
        Ok(Self {
            state: Arc::new(Mutex::new(BrokerState {
                credential_reference_id: config.connector.credential_reference_id().clone(),
                app_id: config.connector.app_id(),
                installation_id: config.connector.installation_id(),
                repository: config.connector.repository().clone(),
                api_base_url: config.api_base_url.clone(),
                private_key,
                webhook_secret,
                token: None,
                agent,
            })),
        })
    }

    fn installation_token(&self) -> Result<(Vec<u8>, u64), GateError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| GateError::new(GateErrorCode::InvalidCredential))?;
        let now = system_now_millis()?;
        if let Some(token) = &state.token
            && token.expires_at_millis.saturating_sub(now)
                > INSTALLATION_TOKEN_REFRESH_MARGIN_MILLIS
        {
            return Ok((token.bytes.clone(), token.expires_at_millis));
        }
        let token = issue_installation_token(&state, now)?;
        let result = (token.bytes.clone(), token.expires_at_millis);
        state.token = Some(token);
        Ok(result)
    }

    fn secret_needles(&self) -> Result<Vec<Vec<u8>>, GateError> {
        let state = self
            .state
            .lock()
            .map_err(|_| GateError::new(GateErrorCode::InvalidCredential))?;
        let mut values = vec![state.private_key.expose().to_vec()];
        values.extend(
            state
                .private_key
                .expose()
                .split(|byte| *byte == b'\n')
                .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
                .filter(|line| line.len() >= 32 && !line.starts_with(b"-----"))
                .map(<[u8]>::to_vec),
        );
        values.push(state.webhook_secret.expose().to_vec());
        if let Some(token) = &state.token {
            values.push(token.bytes.clone());
        }
        Ok(values)
    }
}

fn github_agent(roots: &GitHubTlsRoots) -> ureq::Agent {
    let roots = match roots {
        GitHubTlsRoots::WebPki => ureq::tls::RootCerts::WebPki,
        GitHubTlsRoots::Specific(values) => values
            .iter()
            .map(|value| ureq::tls::Certificate::from_der(value).to_owned())
            .collect::<Vec<_>>()
            .into(),
    };
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(30)))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(roots)
                .use_sni(true)
                .disable_verification(false)
                .build(),
        )
        .build()
        .into()
}

fn issue_installation_token(
    state: &BrokerState,
    now_millis: u64,
) -> Result<CachedInstallationToken, GateError> {
    let jwt = app_jwt(state, now_millis)?;
    validate_installation(state, &jwt)?;
    let repository_name = state
        .repository
        .0
        .split_once('/')
        .map(|(_, repository)| repository)
        .ok_or_else(|| GateError::new(GateErrorCode::InvalidConfiguration))?;
    let permissions = REQUIRED_PERMISSION_PAIRS
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let response = github_json_request(
        state,
        "POST",
        &format!(
            "app/installations/{}/access_tokens",
            state.installation_id.get()
        ),
        &jwt,
        Some(&json!({
            "repositories": [repository_name],
            "permissions": permissions,
        })),
        &[201],
    )?;
    let response: InstallationTokenResponse = serde_json::from_value(response)
        .map_err(|_| GateError::new(GateErrorCode::GitHubResponse))?;
    let expires_at = OffsetDateTime::parse(&response.expires_at, &Rfc3339)
        .map_err(|_| GateError::new(GateErrorCode::GitHubResponse))?;
    let expires_at_millis = u64::try_from(expires_at.unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| GateError::new(GateErrorCode::GitHubResponse))?;
    let expected_permissions = REQUIRED_PERMISSION_PAIRS
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect::<BTreeMap<_, _>>();
    if response.repository_selection != "selected"
        || response.permissions != expected_permissions
        || response.repositories.len() != 1
        || response.repositories[0].full_name != state.repository.0
        || expires_at_millis <= now_millis + INSTALLATION_TOKEN_REFRESH_MARGIN_MILLIS
        || expires_at_millis.saturating_sub(now_millis) > INSTALLATION_TOKEN_MAX_LIFETIME_MILLIS
        || response.token.is_empty()
        || response.token.len() > 4_096
        || !response
            .token
            .bytes()
            .all(|byte| matches!(byte, 0x21..=0x7e))
    {
        return Err(GateError::new(GateErrorCode::GitHubInstallationScope));
    }
    Ok(CachedInstallationToken {
        bytes: response.token.into_bytes(),
        expires_at_millis,
    })
}

fn app_jwt(state: &BrokerState, now_millis: u64) -> Result<String, GateError> {
    let now_seconds = now_millis / 1_000;
    let claims = GitHubAppClaims {
        iat: now_seconds.saturating_sub(60),
        exp: now_seconds.saturating_add(540),
        iss: state.app_id.get().to_string(),
    };
    let key = EncodingKey::from_rsa_pem(state.private_key.expose())
        .map_err(|_| GateError::new(GateErrorCode::InvalidCredential))?;
    encode(&Header::new(Algorithm::RS256), &claims, &key)
        .map_err(|_| GateError::new(GateErrorCode::GitHubAuthentication))
}

fn validate_installation(state: &BrokerState, jwt: &str) -> Result<(), GateError> {
    let value = github_json_request(
        state,
        "GET",
        &format!("app/installations/{}", state.installation_id.get()),
        jwt,
        None,
        &[200],
    )?;
    let id = value.get("id").and_then(Value::as_u64);
    let app_id = value.get("app_id").and_then(Value::as_u64);
    let selection = value.get("repository_selection").and_then(Value::as_str);
    let suspended = value.get("suspended_at").is_some_and(Value::is_null);
    let permissions = value
        .get("permissions")
        .and_then(Value::as_object)
        .and_then(string_map);
    let events = value
        .get("events")
        .and_then(Value::as_array)
        .and_then(|values| string_set(values));
    let expected_permissions = REQUIRED_PERMISSION_PAIRS
        .into_iter()
        .map(|(key, permission)| (key.to_owned(), permission.to_owned()))
        .collect::<BTreeMap<_, _>>();
    let expected_events = REQUIRED_EVENTS
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if id != Some(state.installation_id.get())
        || app_id != Some(state.app_id.get())
        || selection != Some("selected")
        || !suspended
        || permissions.as_ref() != Some(&expected_permissions)
        || events.as_ref() != Some(&expected_events)
    {
        return Err(GateError::new(GateErrorCode::GitHubInstallationScope));
    }
    Ok(())
}

fn string_map(value: &serde_json::Map<String, Value>) -> Option<BTreeMap<String, String>> {
    value
        .iter()
        .map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
        .collect()
}

fn string_set(values: &[Value]) -> Option<BTreeSet<String>> {
    values
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn github_json_request(
    state: &BrokerState,
    method: &str,
    path: &str,
    bearer: &str,
    body: Option<&Value>,
    accepted: &[u16],
) -> Result<Value, GateError> {
    let url = format!("{}{}", state.api_base_url, path.trim_start_matches('/'));
    let authorization = format!("Bearer {bearer}");
    let response = match (method, body) {
        ("GET", None) => state
            .agent
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", &authorization)
            .header("User-Agent", "WinWinCode-GitHub-Live-Gate")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call(),
        ("POST", Some(value)) => state
            .agent
            .post(&url)
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", &authorization)
            .header("User-Agent", "WinWinCode-GitHub-Live-Gate")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send_json(value),
        _ => return Err(GateError::new(GateErrorCode::InvalidConfiguration)),
    }
    .map_err(|_| GateError::new(GateErrorCode::GitHubAuthentication))?;
    if !accepted.contains(&response.status().as_u16()) {
        return Err(GateError::new(GateErrorCode::GitHubAuthentication));
    }
    let bytes = response
        .into_body()
        .with_config()
        .limit(MAX_GITHUB_RESPONSE_BYTES)
        .read_to_vec()
        .map_err(|_| GateError::new(GateErrorCode::GitHubResponse))?;
    serde_json::from_slice(&bytes).map_err(|_| GateError::new(GateErrorCode::GitHubResponse))
}

impl GitHubCredentialPort for SharedGitHubCredentialBroker {
    fn resolve_webhook_secret(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<GitHubWebhookSecret, GitHubCredentialError> {
        let state = self
            .state
            .lock()
            .map_err(|_| GitHubCredentialError::new(GitHubCredentialErrorKind::Unavailable))?;
        if reference != &state.credential_reference_id {
            return Err(GitHubCredentialError::new(
                GitHubCredentialErrorKind::PermissionDenied,
            ));
        }
        GitHubWebhookSecret::try_new(state.webhook_secret.expose())
            .map_err(|_| GitHubCredentialError::new(GitHubCredentialErrorKind::Unavailable))
    }

    fn resolve_installation_token(
        &mut self,
        reference: &CredentialReferenceId,
        app_id: GitHubAppId,
        installation_id: GitHubInstallationId,
    ) -> Result<GitHubInstallationToken, GitHubCredentialError> {
        {
            let state = self
                .state
                .lock()
                .map_err(|_| GitHubCredentialError::new(GitHubCredentialErrorKind::Unavailable))?;
            if reference != &state.credential_reference_id
                || app_id != state.app_id
                || installation_id != state.installation_id
            {
                return Err(GitHubCredentialError::new(
                    GitHubCredentialErrorKind::PermissionDenied,
                ));
            }
        }
        let (token, expires_at_millis) = self
            .installation_token()
            .map_err(|_| GitHubCredentialError::new(GitHubCredentialErrorKind::Unavailable))?;
        let repository = self
            .state
            .lock()
            .map_err(|_| GitHubCredentialError::new(GitHubCredentialErrorKind::Unavailable))?
            .repository
            .clone();
        GitHubInstallationToken::try_new(
            token,
            app_id,
            installation_id,
            repository,
            GitHubInstallationPermissions::new(
                GitHubPermission::Write,
                GitHubPermission::Write,
                GitHubPermission::Write,
                GitHubPermission::Write,
            ),
            expires_at_millis,
        )
        .map_err(|_| GitHubCredentialError::new(GitHubCredentialErrorKind::Unavailable))
    }
}

impl GitHubCredentialResolver for SharedGitHubCredentialBroker {
    fn resolve(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<GitHubCredential, CredentialResolutionError> {
        {
            let state = self
                .state
                .lock()
                .map_err(|_| CredentialResolutionError::unavailable())?;
            if reference != &state.credential_reference_id {
                return Err(CredentialResolutionError::permission_denied());
            }
        }
        let (token, _) = self
            .installation_token()
            .map_err(|_| CredentialResolutionError::unavailable())?;
        GitHubCredential::try_new("github", token)
            .map_err(|_| CredentialResolutionError::unavailable())
    }
}

#[derive(Clone, Copy)]
struct SystemClock;

impl GitHubClock for SystemClock {
    fn now_millis(&self) -> u64 {
        system_now_millis().unwrap_or_default()
    }
}

#[derive(Clone)]
struct DeliveryCommandMapper {
    command: DeliveryCreateCommand,
    issue_number: u64,
}

impl GitHubEventMapperPort for DeliveryCommandMapper {
    fn map_event(
        &mut self,
        _authority: &winwincode_integration::ConnectorAuthority,
        event: &GitHubInboundEvent<'_>,
    ) -> Result<NormalizedInboundEvent, ConnectorCallError> {
        let valid = event.event_type() == "issues"
            && matches!(event.action(), "opened" | "edited" | "reopened" | "labeled")
            && event
                .payload()
                .pointer("/issue/number")
                .and_then(Value::as_u64)
                == Some(self.issue_number);
        if !valid {
            return Err(ConnectorCallError::try_new(
                ConnectorCallErrorKind::Permanent,
                "GITHUB_ISSUE_COMMAND_INVALID",
            )
            .expect("static connector error"));
        }
        let bytes = serde_json::to_vec(&self.command).map_err(|_| {
            ConnectorCallError::try_new(
                ConnectorCallErrorKind::Permanent,
                "GITHUB_ISSUE_COMMAND_INVALID",
            )
            .expect("static connector error")
        })?;
        NormalizedInboundEvent::try_new("delivery.create", bytes).map_err(|_| {
            ConnectorCallError::try_new(
                ConnectorCallErrorKind::Permanent,
                "GITHUB_ISSUE_COMMAND_INVALID",
            )
            .expect("static connector error")
        })
    }
}

#[derive(Clone, Default)]
struct RecordingPublicationAudit {
    decisions: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl RecordingPublicationAudit {
    fn bytes(&self) -> Result<Vec<u8>, GateError> {
        let decisions = self
            .decisions
            .lock()
            .map_err(|_| GateError::new(GateErrorCode::DurableState))?;
        let mut bytes = Vec::new();
        for decision in decisions.iter() {
            bytes.extend_from_slice(decision);
            bytes.push(b'\n');
        }
        Ok(bytes)
    }
}

impl PublicationPolicyAudit for RecordingPublicationAudit {
    fn record(
        &mut self,
        decision: &PublicationPolicyDecision,
    ) -> Result<(), PublicationPolicyAuditError> {
        let bytes =
            serde_json::to_vec(decision).map_err(|_| PublicationPolicyAuditError::unavailable())?;
        self.decisions
            .lock()
            .map_err(|_| PublicationPolicyAuditError::unavailable())?
            .push(bytes);
        Ok(())
    }
}

fn register_connector(
    framework: &mut IntegrationFramework,
    config: &ValidatedConfig,
    occurred_at_millis: u64,
) -> Result<(), GateError> {
    framework
        .register_connector(
            &ConnectorRegistration::try_new(
                config.connector.integration_id().clone(),
                config.integration_scope.clone(),
                ConnectorProtocol::try_new(GITHUB_CONNECTOR_PROTOCOL)
                    .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?,
                config.connector.credential_reference_id().clone(),
                occurred_at_millis,
            )
            .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?,
        )
        .map_err(|_| GateError::new(GateErrorCode::DurableState))?;
    Ok(())
}

fn receive_issue_webhook(
    framework: &mut IntegrationFramework,
    config: &ValidatedConfig,
    broker: &SharedGitHubCredentialBroker,
) -> Result<(), GateError> {
    let factory = GitHubWebhookRequestFactory::new(config.connector.clone());
    let request = factory
        .build(
            config.integration_scope.clone(),
            GitHubWebhookHeaders::try_new(
                config.webhook.delivery_id.clone(),
                config.webhook.event_type.clone(),
                config.webhook.signature_256.as_bytes(),
            )
            .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?,
            config.webhook_payload.clone(),
            config.webhook.received_at_millis,
        )
        .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?;
    let mut verifier = GitHubWebhookVerifier::new(config.connector.clone(), broker.clone());
    let mut connector = GitHubEnterpriseConnector::try_new(
        config.connector.clone(),
        broker.clone(),
        DeliveryCommandMapper {
            command: config.delivery_command.clone(),
            issue_number: config.webhook.issue_number,
        },
        SystemClock,
    )
    .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?;
    let dispatch_count_before = framework
        .storage()
        .inbound_dispatches(
            &config.integration_scope,
            config.connector.integration_id(),
            0,
            10,
        )
        .map_err(|_| GateError::new(GateErrorCode::DurableState))?
        .len();
    if dispatch_count_before > 1 {
        return Err(GateError::new(GateErrorCode::DurableState));
    }
    let first = framework
        .receive_webhook(&request, &mut verifier, &mut connector)
        .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?;
    let replay = framework
        .receive_webhook(&request, &mut verifier, &mut connector)
        .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?;
    if first.status() != InboundStatus::Accepted
        || first.idempotent_replay() != (dispatch_count_before == 1)
        || replay.status() != InboundStatus::Accepted
        || !replay.idempotent_replay()
    {
        return Err(GateError::new(GateErrorCode::CanonicalPath));
    }
    let dispatches = framework
        .storage()
        .inbound_dispatches(
            &config.integration_scope,
            config.connector.integration_id(),
            0,
            10,
        )
        .map_err(|_| GateError::new(GateErrorCode::DurableState))?;
    if dispatches.len() != 1 || dispatches[0].command_name() != "delivery.create" {
        return Err(GateError::new(GateErrorCode::CanonicalPath));
    }
    let command: DeliveryCreateCommand = serde_json::from_slice(dispatches[0].command_payload())
        .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?;
    if command != config.delivery_command
        || command.payload.delivery_id != *config.authorization.binding().delivery_id()
    {
        return Err(GateError::new(GateErrorCode::CanonicalPath));
    }
    Ok(())
}

fn publication_policy_context(
    config: &ValidatedConfig,
    observed_at_millis: u64,
) -> Result<PublicationPolicyContext, GateError> {
    let evidence = PublicationPolicyEvidence::try_from_current_facts(
        &config.authorization,
        true,
        true,
        observed_at_millis,
    )
    .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?;
    PublicationPolicyContext::try_new(
        config.requester.clone(),
        config.publication_request_id.clone(),
        config.publication_scope.clone(),
        PublicationPolicyOrigin::local("github-live-gate")
            .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?,
        evidence,
    )
    .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))
}

fn publication_command_context(
    config: &ValidatedConfig,
    occurred_at_millis: u64,
) -> Result<PublicationCommandContext, GateError> {
    let digest = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                config.publication_command.publication_id(),
                config.authorization.publication_set_sha256(),
                &config.publication_request_id,
            ))
            .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?
        )
    ));
    PublicationCommandContext::try_new(
        ReceiptIdentity::new(
            ReceiptActorKey::from_encoded(
                serde_json::to_vec(&config.requester)
                    .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?,
            )
            .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?,
            ReceiptScopeKey::from_encoded(
                serde_json::to_vec(&config.repository_scope)
                    .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?,
            )
            .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?,
            config.publication_request_id.clone(),
        )
        .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?,
        digest,
        0,
        occurred_at_millis,
    )
    .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))
}

fn publish_canonical_set(
    state_directory: &Path,
    config: &ValidatedConfig,
    broker: SharedGitHubCredentialBroker,
    audit: RecordingPublicationAudit,
) -> Result<u64, GateError> {
    let observed_at_millis = system_now_millis()?;
    if observed_at_millis < config.authorization.approved_at_millis()
        || observed_at_millis.saturating_sub(config.authorization.approved_at_millis())
            > config.policy_max_approval_age()
    {
        return Err(GateError::new(GateErrorCode::InvalidConfiguration));
    }
    let context = publication_command_context(config, observed_at_millis)?;
    let policy_context = publication_policy_context(config, observed_at_millis)?;
    let publication_root = state_directory.join("publication");
    let mut storage = SqliteStorage::open(&publication_root)
        .map_err(|_| GateError::new(GateErrorCode::DurableState))?;
    let mut adapter = config
        .connector
        .publication_adapter(broker)
        .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?;
    {
        let mut coordinator = winwincode_publication::PublicationCoordinator::new(
            PublicationLedger::new(&mut storage),
            &mut adapter,
            Box::new(audit.clone()),
        );
        coordinator
            .publish(
                &context,
                &config.publication_command,
                &config.authorization,
                &config.attribution,
                &policy_context,
                &config.policy,
            )
            .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?;
    }
    let resume_at = observed_at_millis.saturating_add(1);
    let resume_context = publication_policy_context(config, resume_at)?;
    let published = winwincode_publication::PublicationCoordinator::new(
        PublicationLedger::new(&mut storage),
        &mut adapter,
        Box::new(audit),
    )
    .resume(
        config.publication_command.publication_id(),
        resume_at,
        &resume_context,
        &config.policy,
    )
    .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?;
    if published.state() != PublicationState::Published {
        return Err(GateError::new(GateErrorCode::CanonicalPath));
    }
    let resource = published
        .resource()
        .ok_or_else(|| GateError::new(GateErrorCode::CanonicalPath))?;
    if resource.kind() != PublicationResourceKind::GitHubPullRequest
        || resource.repository() != config.connector.repository().0
    {
        return Err(GateError::new(GateErrorCode::CanonicalPath));
    }
    let number = resource.number();
    Box::new(storage)
        .close()
        .map_err(|_| GateError::new(GateErrorCode::DurableState))?;
    Ok(number)
}

impl ValidatedConfig {
    fn policy_max_approval_age(&self) -> u64 {
        self.max_approval_age_millis
    }
}

fn outbound_request(
    config: &ValidatedConfig,
    operation_key: &str,
    operation_name: &str,
    payload: &Value,
    now_millis: u64,
) -> Result<OutboundRequest, GateError> {
    OutboundRequest::try_new(
        config.connector.integration_id().clone(),
        config.integration_scope.clone(),
        IntegrationOperationKey::derive(operation_key)
            .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?,
        operation_name,
        serde_json::to_vec(payload).map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?,
        RetryPolicy::try_new(5, 1_000, 60_000)
            .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?,
        now_millis,
    )
    .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))
}

fn lease_id(tail: char) -> Result<IntegrationLeaseId, GateError> {
    IntegrationLeaseId::try_new(format!("igl_{}", tail.to_string().repeat(26)))
        .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))
}

fn deliver_review_and_check(
    state_directory: &Path,
    config: &ValidatedConfig,
    broker: SharedGitHubCredentialBroker,
    pull_request_number: u64,
) -> Result<(), GateError> {
    let integration_root = state_directory.join("integration");
    let now = system_now_millis()?;
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&integration_root)
            .map_err(|_| GateError::new(GateErrorCode::DurableState))?,
    );
    register_connector(&mut framework, config, config.webhook.received_at_millis)?;
    let mut connector = GitHubEnterpriseConnector::try_new(
        config.connector.clone(),
        broker,
        DeliveryCommandMapper {
            command: config.delivery_command.clone(),
            issue_number: config.webhook.issue_number,
        },
        SystemClock,
    )
    .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?;
    let review_key = format!(
        "delivery:{}:publication:{}:review",
        config.authorization.binding().delivery_id().0,
        config.publication_command.publication_id().0
    );
    let review = outbound_request(
        config,
        &review_key,
        "github.pull_request.review.v1",
        &json!({
            "pull_number": pull_request_number,
            "body": format!(
                "WinWinCode Delivery {} published the approved fact set {}.",
                config.authorization.binding().delivery_id().0,
                config.authorization.publication_set_sha256().0,
            ),
            "event": "COMMENT",
            "commit_id": config.authorization.candidate_commit_id(),
        }),
        now,
    )?;
    let review_enqueue = framework
        .enqueue_outbound(&review)
        .map_err(|_| GateError::new(GateErrorCode::DurableState))?;
    let mut recovered = if review_enqueue.operation().state() == OutboundOperationState::Delivered {
        framework
    } else {
        let claim_at = now.saturating_add(60_000);
        let lease_expires_at = claim_at.saturating_add(5_000);
        let abandoned = framework
            .storage_mut()
            .claim_due(
                &config.integration_scope,
                config.connector.integration_id(),
                claim_at,
                lease_id('R')?,
                lease_expires_at,
            )
            .map_err(|_| GateError::new(GateErrorCode::DurableState))?
            .ok_or_else(|| GateError::new(GateErrorCode::CanonicalPath))?;
        let remote = connector
            .deliver_outbound(&abandoned)
            .map_err(|_| GateError::new(GateErrorCode::CanonicalPath))?;
        if !remote.remote_write_performed() {
            return Err(GateError::new(GateErrorCode::CanonicalPath));
        }
        drop(framework);
        let mut restarted = IntegrationFramework::new(
            IntegrationStorage::open(&integration_root)
                .map_err(|_| GateError::new(GateErrorCode::DurableState))?,
        );
        let recovered_result = restarted
            .deliver_next(
                &config.integration_scope,
                config.connector.integration_id(),
                lease_expires_at,
                lease_id('S')?,
                lease_expires_at.saturating_add(5_000),
                &mut connector,
            )
            .map_err(|_| GateError::new(GateErrorCode::DurableState))?
            .ok_or_else(|| GateError::new(GateErrorCode::CanonicalPath))?;
        let OutboundAttemptResult::Delivered(receipt) = recovered_result else {
            return Err(GateError::new(GateErrorCode::CanonicalPath));
        };
        if receipt.remote_write_performed() != Some(false)
            || receipt.operation().state() != OutboundOperationState::Delivered
        {
            return Err(GateError::new(GateErrorCode::CanonicalPath));
        }
        restarted
    };
    let check_key = format!(
        "delivery:{}:publication:{}:check",
        config.authorization.binding().delivery_id().0,
        config.publication_command.publication_id().0
    );
    let check_at = now.saturating_add(120_000);
    let check = outbound_request(
        config,
        &check_key,
        "github.check_run.upsert.v1",
        &json!({
            "name": "WinWinCode Delivery",
            "head_sha": config.authorization.candidate_commit_id(),
            "status": "completed",
            "conclusion": "success",
            "title": "Approved Delivery published",
            "summary": format!(
                "Delivery {} and Publication {} converged through the canonical paths.",
                config.authorization.binding().delivery_id().0,
                config.publication_command.publication_id().0,
            ),
            "details_url": null,
        }),
        check_at,
    )?;
    let check_enqueue = recovered
        .enqueue_outbound(&check)
        .map_err(|_| GateError::new(GateErrorCode::DurableState))?;
    if check_enqueue.operation().state() != OutboundOperationState::Delivered {
        let result = recovered
            .deliver_next(
                &config.integration_scope,
                config.connector.integration_id(),
                check_at,
                lease_id('T')?,
                check_at.saturating_add(5_000),
                &mut connector,
            )
            .map_err(|_| GateError::new(GateErrorCode::DurableState))?
            .ok_or_else(|| GateError::new(GateErrorCode::CanonicalPath))?;
        if !matches!(result, OutboundAttemptResult::Delivered(_)) {
            return Err(GateError::new(GateErrorCode::CanonicalPath));
        }
    }
    if !recovered
        .enqueue_outbound(&check)
        .map_err(|_| GateError::new(GateErrorCode::DurableState))?
        .idempotent_replay()
    {
        return Err(GateError::new(GateErrorCode::CanonicalPath));
    }
    Ok(())
}

fn scan_for_secret_leak(
    state_directory: &Path,
    extra_outputs: &[Vec<u8>],
    needles: &[Vec<u8>],
) -> Result<(), GateError> {
    let needles = needles
        .iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    for output in extra_outputs {
        if needles
            .iter()
            .any(|needle| find_bytes(output, needle).is_some())
        {
            return Err(GateError::new(GateErrorCode::SecretLeak));
        }
    }
    let mut pending = vec![state_directory.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path).map_err(|_| GateError::new(GateErrorCode::DurableState))? {
            let entry = entry.map_err(|_| GateError::new(GateErrorCode::DurableState))?;
            let file_type = entry
                .file_type()
                .map_err(|_| GateError::new(GateErrorCode::DurableState))?;
            if file_type.is_symlink() {
                return Err(GateError::new(GateErrorCode::DurableState));
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                let bytes = fs::read(entry.path())
                    .map_err(|_| GateError::new(GateErrorCode::DurableState))?;
                if needles
                    .iter()
                    .any(|needle| find_bytes(&bytes, needle).is_some())
                {
                    return Err(GateError::new(GateErrorCode::SecretLeak));
                }
            } else {
                return Err(GateError::new(GateErrorCode::DurableState));
            }
        }
    }
    Ok(())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn system_now_millis() -> Result<u64, GateError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| GateError::new(GateErrorCode::InvalidConfiguration))?
        .as_millis();
    u64::try_from(millis)
        .ok()
        .filter(|value| *value > 0 && *value <= MAX_SAFE_INTEGER)
        .ok_or_else(|| GateError::new(GateErrorCode::InvalidConfiguration))
}

#[derive(Clone)]
struct BrokerHttpReply {
    status: u16,
    body: Value,
}

struct BrokerTlsFixture {
    endpoint: String,
    certificate_der: Vec<u8>,
    requests: mpsc::Receiver<Vec<u8>>,
    response_count: usize,
    server: thread::JoinHandle<()>,
}

impl BrokerTlsFixture {
    fn start(replies: Vec<BrokerHttpReply>) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("generate broker TLS certificate");
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .expect("broker TLS server config");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind broker TLS fixture");
        let address = listener.local_addr().expect("broker TLS address");
        let response_count = replies.len();
        let (sender, requests) = mpsc::channel();
        let server = thread::spawn(move || {
            for reply in replies {
                let (socket, _) = listener.accept().expect("accept broker TLS request");
                let connection =
                    ServerConnection::new(Arc::new(config.clone())).expect("TLS connection");
                let mut stream = StreamOwned::new(connection, socket);
                sender
                    .send(read_broker_http_request(&mut stream))
                    .expect("record broker request");
                write_broker_http_reply(&mut stream, &reply);
            }
        });
        Self {
            endpoint: format!("https://localhost:{}/api/v3/", address.port()),
            certificate_der: cert.der().to_vec(),
            requests,
            response_count,
            server,
        }
    }

    fn finish(self) -> Vec<Vec<u8>> {
        self.server.join().expect("join broker TLS fixture");
        (0..self.response_count)
            .map(|_| self.requests.recv().expect("captured broker request"))
            .collect()
    }
}

fn read_broker_http_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4_096];
    loop {
        let count = stream.read(&mut buffer).expect("read broker request");
        assert_ne!(count, 0, "broker request closed before body");
        request.extend_from_slice(&buffer[..count]);
        let Some(header_end) = find_bytes(&request, b"\r\n\r\n") else {
            continue;
        };
        let length = broker_content_length(&request[..header_end]);
        if request.len() >= header_end + 4 + length {
            return request;
        }
    }
}

fn broker_content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0)
}

fn broker_http_body(request: &[u8]) -> &[u8] {
    let header_end = find_bytes(request, b"\r\n\r\n").expect("broker HTTP header terminator");
    &request[header_end + 4..]
}

fn write_broker_http_reply(
    stream: &mut StreamOwned<ServerConnection, TcpStream>,
    reply: &BrokerHttpReply,
) {
    let body = serde_json::to_vec(&reply.body).expect("broker reply JSON");
    let reason = match reply.status {
        200 => "OK",
        201 => "Created",
        _ => "Unprocessable Entity",
    };
    write!(
        stream,
        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        body.len()
    )
    .expect("write broker response headers");
    stream.write_all(&body).expect("write broker response body");
    stream.flush().expect("flush broker response");
}

fn rfc3339_millis(timestamp_millis: u64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp_millis) * 1_000_000)
        .expect("fixture timestamp")
        .format(&Rfc3339)
        .expect("fixture RFC 3339 timestamp")
}

fn run_live_gate() -> Result<(), GateError> {
    let environment = GateEnvironment::from_process()?;
    let payload = read_regular_file(
        &environment.webhook_payload_file,
        MAX_WEBHOOK_PAYLOAD_BYTES,
        false,
    )?;
    let config = LiveConfigFile::read(&environment)?.validate(payload)?;
    let state_directory = prepare_state_directory(&environment.state_directory)?;
    let broker = SharedGitHubCredentialBroker::open(&config, &environment)?;
    run_configured_gate(&state_directory, &config, &broker)
}

fn run_configured_gate(
    state_directory: &Path,
    config: &ValidatedConfig,
    broker: &SharedGitHubCredentialBroker,
) -> Result<(), GateError> {
    let state_directory = prepare_state_directory(state_directory)?;
    let integration_root = state_directory.join("integration");
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&integration_root)
            .map_err(|_| GateError::new(GateErrorCode::DurableState))?,
    );
    register_connector(&mut framework, config, config.webhook.received_at_millis)?;
    receive_issue_webhook(&mut framework, config, broker)?;
    drop(framework);
    let publication_audit = RecordingPublicationAudit::default();
    let pull_request_number = publish_canonical_set(
        &state_directory,
        config,
        broker.clone(),
        publication_audit.clone(),
    )?;
    deliver_review_and_check(
        &state_directory,
        config,
        broker.clone(),
        pull_request_number,
    )?;
    let integration = IntegrationFramework::new(
        IntegrationStorage::open(&integration_root)
            .map_err(|_| GateError::new(GateErrorCode::DurableState))?,
    );
    let integration_audit = serde_json::to_vec(
        &integration
            .storage()
            .audit_facts(
                &config.integration_scope,
                config.connector.integration_id(),
                0,
                200,
            )
            .map_err(|_| GateError::new(GateErrorCode::DurableState))?,
    )
    .map_err(|_| GateError::new(GateErrorCode::DurableState))?;
    drop(integration);
    let publication_audit = publication_audit.bytes()?;
    scan_for_secret_leak(
        &state_directory,
        &[integration_audit, publication_audit],
        &broker.secret_needles()?,
    )
}

#[test]
#[ignore = "requires an explicit GitHub App sandbox, captured Issue webhook, secure credential files, and minimal repository installation"]
fn live_github_app_issue_delivery_publication_trace() {
    run_live_gate().expect("GitHub App live gate must pass without exposing provider diagnostics");
}

#[test]
fn app_broker_uses_rsa_jwt_narrowed_installation_token_and_verified_tls() {
    let now = system_now_millis().expect("fixture clock");
    let expires_at = now.saturating_add(3_600_000);
    let permissions = REQUIRED_PERMISSION_PAIRS
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let installation_token = "github-installation-token-live-gate-fixture";
    let fixture = BrokerTlsFixture::start(vec![
        BrokerHttpReply {
            status: 200,
            body: json!({
                "id": 8,
                "app_id": 7,
                "repository_selection": "selected",
                "suspended_at": null,
                "permissions": permissions,
                "events": REQUIRED_EVENTS,
            }),
        },
        BrokerHttpReply {
            status: 201,
            body: json!({
                "token": installation_token,
                "expires_at": rfc3339_millis(expires_at),
                "permissions": REQUIRED_PERMISSION_PAIRS
                    .into_iter()
                    .collect::<BTreeMap<_, _>>(),
                "repository_selection": "selected",
                "repositories": [{"full_name": "example/widget"}],
            }),
        },
    ]);
    let private_key = KeyPair::generate_for(&PKCS_RSA_SHA256)
        .expect("generate RSA app key")
        .serialize_pem()
        .into_bytes();
    let webhook_secret = b"github-webhook-secret-live-gate-fixture".to_vec();
    let broker = SharedGitHubCredentialBroker {
        state: Arc::new(Mutex::new(BrokerState {
            credential_reference_id: CredentialReferenceId(
                "crd_00000000000000000000000001".to_owned(),
            ),
            app_id: GitHubAppId::try_new(7).expect("app id"),
            installation_id: GitHubInstallationId::try_new(8).expect("installation id"),
            repository: GitHubRepositorySlug("example/widget".to_owned()),
            api_base_url: fixture.endpoint.clone(),
            private_key: SecretBytes(private_key.clone()),
            webhook_secret: SecretBytes(webhook_secret.clone()),
            token: None,
            agent: github_agent(&GitHubTlsRoots::Specific(vec![
                fixture.certificate_der.clone(),
            ])),
        })),
    };

    let (first_token, first_expiration) = broker.installation_token().expect("installation token");
    let (cached_token, cached_expiration) = broker.installation_token().expect("cached token");
    assert!(first_token == installation_token.as_bytes());
    assert!(cached_token == first_token);
    assert_eq!(cached_expiration, first_expiration);

    let requests = fixture.finish();
    assert_eq!(requests.len(), 2);
    let first = String::from_utf8_lossy(&requests[0]).to_ascii_lowercase();
    assert!(first.starts_with("get /api/v3/app/installations/8 http/1.1\r\n"));
    let authorization = first
        .lines()
        .find_map(|line| line.strip_prefix("authorization: bearer "))
        .expect("JWT bearer header");
    assert_eq!(authorization.split('.').count(), 3);
    assert!(
        String::from_utf8_lossy(&requests[1])
            .to_ascii_lowercase()
            .starts_with("post /api/v3/app/installations/8/access_tokens http/1.1\r\n")
    );
    let body: Value =
        serde_json::from_slice(broker_http_body(&requests[1])).expect("token request JSON");
    assert_eq!(body.get("repositories"), Some(&json!(["widget"])));
    assert_eq!(
        body.get("permissions"),
        Some(&json!(
            REQUIRED_PERMISSION_PAIRS
                .into_iter()
                .collect::<BTreeMap<_, _>>()
        ))
    );
    for request in &requests {
        assert!(find_bytes(request, &private_key).is_none());
        assert!(find_bytes(request, &webhook_secret).is_none());
        assert!(find_bytes(request, installation_token.as_bytes()).is_none());
    }
}

#[test]
fn live_config_rejects_embedded_credentials_and_unknown_fields() {
    assert_eq!(
        reject_embedded_credentials(br#"{"token":"not-allowed"}"#)
            .expect_err("direct token must be rejected")
            .code(),
        GateErrorCode::InvalidConfiguration
    );
    assert_eq!(
        reject_embedded_credentials(b"-----BEGIN PRIVATE KEY-----")
            .expect_err("private key must be a credential reference")
            .code(),
        GateErrorCode::InvalidConfiguration
    );
    let mut config = json!({
        "schemaVersion": 1,
        "apiBaseUrl": "https://api.github.com",
        "integrationId": "int_00000000000000000000000001",
        "credentialReferenceId": "crd_00000000000000000000000001",
        "appId": 1,
        "installationId": 2,
        "repository": "example/widget",
        "scope": {
            "organizationId": "org_00000000000000000000000001",
            "workspaceId": "wsp_00000000000000000000000001",
            "projectId": "prj_00000000000000000000000001",
            "repositoryId": "rep_00000000000000000000000001"
        },
        "webhook": {
            "deliveryId": "provider-delivery",
            "eventType": "issues",
            "signature256": format!("sha256={}", "a".repeat(64)),
            "receivedAtMillis": 1,
            "issueNumber": 7
        },
        "delivery": {
            "deliveryId": "dlv_00000000000000000000000001",
            "requestId": "req_00000000000000000000000001",
            "systemActorId": "sys_00000000000000000000000001",
            "deliveryRevision": 7,
            "deliverySpecId": "spec_00000000000000000000000001",
            "deliverySpecRevision": 1,
            "candidateRef": format!("git-candidate:sha256:{}", "b".repeat(64)),
            "diffSha256": "c".repeat(64),
            "verdictId": "verdict:live:pass",
            "approvalId": "att_00000000000000000000000001",
            "approvalReviewSetSha256": "d".repeat(64),
            "candidateCommitId": "e".repeat(40),
            "artifactId": "art_00000000000000000000000001",
            "artifactDigest": format!("sha256:{}", "f".repeat(64)),
            "approvedBy": "usr_00000000000000000000000001",
            "approvedAtMillis": 1,
            "productSessionId": "psn_00000000000000000000000001"
        },
        "publication": {
            "publicationId": "pub_00000000000000000000000001",
            "requestId": "req_00000000000000000000000002",
            "baseBranch": "main",
            "headRepository": "example/widget",
            "headBranch": "winwincode/live-gate",
            "maxApprovalAgeMillis": 1000
        }
    });
    let payload = serde_json::to_vec(&json!({
        "action": "opened",
        "installation": {"id": 2},
        "issue": {
            "id": 123,
            "number": 7,
            "updated_at": "2026-08-28T12:00:00Z"
        },
        "repository": {"full_name": "example/widget"}
    }))
    .expect("issue payload");
    serde_json::from_value::<LiveConfigFile>(config.clone())
        .expect("strict fixture schema")
        .validate(payload)
        .expect("valid live-gate configuration");
    config["unexpected"] = Value::Bool(true);
    assert!(serde_json::from_value::<LiveConfigFile>(config).is_err());
}

#[test]
fn secret_scan_detects_every_credential_family_without_printing_values() {
    let root = env::temp_dir().join(format!(
        "winwincode-github-live-gate-secret-scan-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    prepare_state_directory(&root).expect("state directory");
    let secret = b"GITHUB_LIVE_GATE_SECRET_SENTINEL".to_vec();
    fs::write(root.join("safe.json"), br#"{"status":"safe"}"#).expect("write safe output");
    scan_for_secret_leak(&root, &[], std::slice::from_ref(&secret)).expect("safe output passes");
    assert_eq!(
        scan_for_secret_leak(
            &root,
            std::slice::from_ref(&secret),
            std::slice::from_ref(&secret)
        )
        .expect_err("in-memory audit leak must fail")
        .code(),
        GateErrorCode::SecretLeak
    );
    fs::write(root.join("leaked.json"), &secret).expect("write leaked output");
    assert_eq!(
        scan_for_secret_leak(&root, &[], &[secret])
            .expect_err("leak must fail")
            .code(),
        GateErrorCode::SecretLeak
    );
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn credential_inputs_require_owner_only_regular_files() {
    let root = env::temp_dir().join(format!(
        "winwincode-github-live-gate-credential-inputs-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("credential fixture directory");
    let credential = root.join("credential");
    fs::write(&credential, b"credential-file-fixture").expect("credential fixture");
    fs::set_permissions(&credential, fs::Permissions::from_mode(0o644))
        .expect("unsafe fixture mode");
    assert_eq!(
        SecretBytes::read(&credential, 1_024)
            .expect_err("group-readable credential must fail")
            .code(),
        GateErrorCode::UnsafeCredentialFile
    );
    fs::set_permissions(&credential, fs::Permissions::from_mode(0o600))
        .expect("owner-only fixture mode");
    drop(SecretBytes::read(&credential, 1_024).expect("owner-only credential"));
    let link = root.join("credential-link");
    std::os::unix::fs::symlink(&credential, &link).expect("credential symlink fixture");
    assert_eq!(
        SecretBytes::read(&link, 1_024)
            .expect_err("credential symlink must fail")
            .code(),
        GateErrorCode::UnsafeCredentialFile
    );
    fs::remove_dir_all(root).expect("cleanup credential fixture");
}
