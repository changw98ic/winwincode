// SPDX-License-Identifier: Apache-2.0

//! GitHub App protocol adapter over the durable Integration Framework.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use winwincode_audit::AuditScope;
use winwincode_domain::{
    CredentialReferenceId, EnterpriseIntegrationId, GitHubRepositorySlug, Sha256Digest,
};
use winwincode_publication::{
    GitHubAdapterConfig, GitHubCredentialResolver, GitHubPublicationAdapter,
};

use crate::model::{MAX_SAFE_INTEGER, validate_integration_id};
use crate::{
    ConnectorAuthority, ConnectorCallError, ConnectorCallErrorKind, ConnectorPort,
    InboundNormalizationContext, InboundWebhookMetadata, InboundWebhookRequest, IntegrationError,
    IntegrationErrorKind, NormalizedInboundEvent, OutboundCallReceipt, OutboundClaim,
    SignatureVerificationError, WebhookSignatureVerifier,
};

/// Canonical Integration Framework protocol identifier for GitHub Apps.
pub const GITHUB_CONNECTOR_PROTOCOL: &str = "github.app.v1";
const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "WinWinCode-GitHub-Enterprise-Connector";
const MAX_RESPONSE_BYTES: u64 = 2 * 1_024 * 1_024;
const PAGE_SIZE: usize = 100;

/// Canonical positive GitHub App identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitHubAppId(u64);

impl GitHubAppId {
    /// Builds an App identity.
    ///
    /// # Errors
    ///
    /// Rejects zero or a value outside the JavaScript-safe integer range.
    pub fn try_new(value: u64) -> Result<Self, IntegrationError> {
        if value == 0 || value > MAX_SAFE_INTEGER {
            return Err(invalid("GitHub App identity is invalid"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Canonical positive GitHub App installation identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitHubInstallationId(u64);

impl GitHubInstallationId {
    /// Builds an installation identity.
    ///
    /// # Errors
    ///
    /// Rejects zero or a value outside the JavaScript-safe integer range.
    pub fn try_new(value: u64) -> Result<Self, IntegrationError> {
        if value == 0 || value > MAX_SAFE_INTEGER {
            return Err(invalid("GitHub installation identity is invalid"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Closed installation permission level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitHubPermission {
    None,
    Read,
    Write,
}

impl GitHubPermission {
    const fn permits(self, required: Self) -> bool {
        matches!(
            (self, required),
            (_, Self::None) | (Self::Read | Self::Write, Self::Read) | (Self::Write, Self::Write)
        )
    }
}

/// Permission snapshot bound to one short-lived installation token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitHubInstallationPermissions {
    issues: GitHubPermission,
    pull_requests: GitHubPermission,
    checks: GitHubPermission,
    contents: GitHubPermission,
}

impl GitHubInstallationPermissions {
    #[must_use]
    pub const fn new(
        issues: GitHubPermission,
        pull_requests: GitHubPermission,
        checks: GitHubPermission,
        contents: GitHubPermission,
    ) -> Self {
        Self {
            issues,
            pull_requests,
            checks,
            contents,
        }
    }
}

/// Explicit TLS root configuration for GitHub.com or GHES.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitHubTlsRoots {
    WebPki,
    Specific(Vec<Vec<u8>>),
}

/// Closed GitHub App/installation/repository configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubConnectorConfig {
    integration_id: EnterpriseIntegrationId,
    credential_reference_id: CredentialReferenceId,
    app_id: GitHubAppId,
    installation_id: GitHubInstallationId,
    repository: GitHubRepositorySlug,
    api_base_url: String,
    tls_roots: GitHubTlsRoots,
    request_timeout: Duration,
    max_lookup_pages: u16,
}

impl GitHubConnectorConfig {
    /// Builds a credential-free GitHub.com or GHES connector configuration.
    ///
    /// # Errors
    ///
    /// Rejects invalid identities, repository scope, URL, TLS roots, or bounds.
    pub fn try_new(
        integration_id: EnterpriseIntegrationId,
        credential_reference_id: CredentialReferenceId,
        app_id: GitHubAppId,
        installation_id: GitHubInstallationId,
        repository: GitHubRepositorySlug,
        api_base_url: impl Into<String>,
        tls_roots: GitHubTlsRoots,
    ) -> Result<Self, IntegrationError> {
        validate_integration_id(&integration_id)?;
        validate_prefixed_id(&credential_reference_id.0, "crd")?;
        validate_repository(&repository.0)?;
        validate_tls_roots(&tls_roots)?;
        let api_base_url = canonical_api_base_url(&api_base_url.into())?;
        Ok(Self {
            integration_id,
            credential_reference_id,
            app_id,
            installation_id,
            repository,
            api_base_url,
            tls_roots,
            request_timeout: Duration::from_secs(30),
            max_lookup_pages: 20,
        })
    }

    #[must_use]
    pub const fn integration_id(&self) -> &EnterpriseIntegrationId {
        &self.integration_id
    }

    #[must_use]
    pub const fn credential_reference_id(&self) -> &CredentialReferenceId {
        &self.credential_reference_id
    }

    #[must_use]
    pub const fn app_id(&self) -> GitHubAppId {
        self.app_id
    }

    #[must_use]
    pub const fn installation_id(&self) -> GitHubInstallationId {
        self.installation_id
    }

    #[must_use]
    pub const fn repository(&self) -> &GitHubRepositorySlug {
        &self.repository
    }

    /// Builds the existing canonical Publication adapter against the same
    /// credential reference and GitHub.com/GHES API boundary.
    ///
    /// # Errors
    ///
    /// Rejects a configuration not accepted by the Publication adapter.
    pub fn publication_adapter<Resolver: GitHubCredentialResolver>(
        &self,
        resolver: Resolver,
    ) -> Result<GitHubPublicationAdapter<Resolver>, IntegrationError> {
        let config = GitHubAdapterConfig::try_new(
            self.credential_reference_id.clone(),
            self.api_base_url.clone(),
        )
        .map_err(|_| invalid("GitHub Publication configuration is invalid"))?;
        Ok(GitHubPublicationAdapter::new(config, resolver))
    }
}

/// Stable secret-resolution error category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitHubCredentialErrorKind {
    Revoked,
    PermissionDenied,
    Unavailable,
}

/// Secret-safe credential resolution error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitHubCredentialError {
    kind: GitHubCredentialErrorKind,
}

impl GitHubCredentialError {
    #[must_use]
    pub const fn new(kind: GitHubCredentialErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(self) -> GitHubCredentialErrorKind {
        self.kind
    }
}

impl fmt::Display for GitHubCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitHub credential could not be resolved")
    }
}

impl std::error::Error for GitHubCredentialError {}

/// Short-lived webhook secret; never serializable or clonable.
pub struct GitHubWebhookSecret(Vec<u8>);

impl GitHubWebhookSecret {
    /// Builds an opaque webhook secret.
    ///
    /// # Errors
    ///
    /// Rejects empty or oversized secret material.
    pub fn try_new(value: impl AsRef<[u8]>) -> Result<Self, IntegrationError> {
        let value = value.as_ref();
        if value.is_empty() || value.len() > 4_096 {
            return Err(invalid("GitHub webhook secret is invalid"));
        }
        Ok(Self(value.to_vec()))
    }

    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for GitHubWebhookSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitHubWebhookSecret([REDACTED])")
    }
}

impl Drop for GitHubWebhookSecret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Short-lived GitHub App installation token and its sealed scope snapshot.
pub struct GitHubInstallationToken {
    token: Vec<u8>,
    app_id: GitHubAppId,
    installation_id: GitHubInstallationId,
    repository: GitHubRepositorySlug,
    permissions: GitHubInstallationPermissions,
    expires_at_millis: u64,
}

impl GitHubInstallationToken {
    /// Builds a short-lived installation token returned by the credential owner.
    ///
    /// # Errors
    ///
    /// Rejects invalid token bytes, repository, or expiry.
    pub fn try_new(
        token: impl AsRef<[u8]>,
        app_id: GitHubAppId,
        installation_id: GitHubInstallationId,
        repository: GitHubRepositorySlug,
        permissions: GitHubInstallationPermissions,
        expires_at_millis: u64,
    ) -> Result<Self, IntegrationError> {
        let token = token.as_ref();
        validate_repository(&repository.0)?;
        if token.is_empty()
            || token.len() > 4_096
            || !token.iter().all(|byte| matches!(byte, 0x21..=0x7e))
            || expires_at_millis == 0
            || expires_at_millis > MAX_SAFE_INTEGER
        {
            return Err(invalid("GitHub installation token is invalid"));
        }
        Ok(Self {
            token: token.to_vec(),
            app_id,
            installation_id,
            repository,
            permissions,
            expires_at_millis,
        })
    }

    fn value(&self) -> &str {
        std::str::from_utf8(&self.token).expect("validated visible ASCII token")
    }
}

impl fmt::Debug for GitHubInstallationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubInstallationToken")
            .field("app_id", &self.app_id)
            .field("installation_id", &self.installation_id)
            .field("repository", &self.repository)
            .field("permissions", &self.permissions)
            .field("expires_at_millis", &self.expires_at_millis)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl Drop for GitHubInstallationToken {
    fn drop(&mut self) {
        self.token.fill(0);
    }
}

/// Credential-owner port for one App credential reference.
pub trait GitHubCredentialPort {
    /// Resolves the webhook secret only for signature verification.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe credential error.
    fn resolve_webhook_secret(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<GitHubWebhookSecret, GitHubCredentialError>;

    /// Resolves one short-lived installation token scoped by App and installation.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe credential error.
    fn resolve_installation_token(
        &mut self,
        reference: &CredentialReferenceId,
        app_id: GitHubAppId,
        installation_id: GitHubInstallationId,
    ) -> Result<GitHubInstallationToken, GitHubCredentialError>;
}

/// Time authority used only for installation token expiry checks.
pub trait GitHubClock {
    fn now_millis(&self) -> u64;
}

/// Exact GitHub webhook headers before raw-body authentication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubWebhookHeaders {
    delivery_id: String,
    event_type: String,
    signature_256: Vec<u8>,
}

impl GitHubWebhookHeaders {
    /// Builds a bounded GitHub webhook header set.
    ///
    /// # Errors
    ///
    /// Rejects missing/oversized delivery, event, or signature headers.
    pub fn try_new(
        delivery_id: impl Into<String>,
        event_type: impl Into<String>,
        signature_256: impl AsRef<[u8]>,
    ) -> Result<Self, IntegrationError> {
        let delivery_id = delivery_id.into();
        let event_type = event_type.into();
        let signature_256 = signature_256.as_ref();
        if delivery_id.is_empty()
            || delivery_id.len() > 128
            || event_type.is_empty()
            || event_type.len() > 64
            || !event_type
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            || signature_256.len() != 71
            || !signature_256.starts_with(b"sha256=")
        {
            return Err(invalid("GitHub webhook headers are invalid"));
        }
        Ok(Self {
            delivery_id,
            event_type,
            signature_256: signature_256.to_vec(),
        })
    }
}

/// Builds Integration Framework requests from GitHub webhook headers/body.
#[derive(Clone, Debug)]
pub struct GitHubWebhookRequestFactory {
    config: GitHubConnectorConfig,
}

impl GitHubWebhookRequestFactory {
    #[must_use]
    pub const fn new(config: GitHubConnectorConfig) -> Self {
        Self { config }
    }

    /// Builds one raw request with resource-local ordering facts.
    ///
    /// # Errors
    ///
    /// Rejects unsupported/non-canonical payloads or mismatched installation/repository scope.
    pub fn build(
        &self,
        scope: AuditScope,
        headers: GitHubWebhookHeaders,
        payload: Vec<u8>,
        received_at_millis: u64,
    ) -> Result<InboundWebhookRequest, IntegrationError> {
        let value: Value = serde_json::from_slice(&payload)
            .map_err(|_| invalid("GitHub webhook payload is invalid"))?;
        validate_webhook_scope(&self.config, &value)?;
        let (ordering_key, sequence) = webhook_ordering(&headers.event_type, &value)?;
        let metadata = InboundWebhookMetadata::try_new(
            headers.event_type,
            headers.delivery_id,
            ordering_key,
            sequence,
            received_at_millis,
        )?;
        InboundWebhookRequest::try_new(
            self.config.integration_id.clone(),
            scope,
            metadata,
            headers.signature_256,
            payload,
        )
    }
}

/// HMAC-SHA256 verifier using only the connector credential reference.
pub struct GitHubWebhookVerifier<Credentials> {
    config: GitHubConnectorConfig,
    credentials: Credentials,
}

impl<Credentials> GitHubWebhookVerifier<Credentials> {
    #[must_use]
    pub const fn new(config: GitHubConnectorConfig, credentials: Credentials) -> Self {
        Self {
            config,
            credentials,
        }
    }

    #[must_use]
    pub fn into_credentials(self) -> Credentials {
        self.credentials
    }
}

impl<Credentials: GitHubCredentialPort> WebhookSignatureVerifier
    for GitHubWebhookVerifier<Credentials>
{
    fn verify(
        &mut self,
        authority: &ConnectorAuthority,
        signature: &[u8],
        payload: &[u8],
    ) -> Result<(), SignatureVerificationError> {
        if !matches_authority(&self.config, authority) {
            return Err(SignatureVerificationError::rejected());
        }
        let secret = self
            .credentials
            .resolve_webhook_secret(authority.credential_reference_id())
            .map_err(signature_credential_error)?;
        let supplied =
            decode_signature(signature).ok_or_else(SignatureVerificationError::rejected)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.bytes())
            .map_err(|_| SignatureVerificationError::rejected())?;
        mac.update(payload);
        mac.verify_slice(&supplied)
            .map_err(|_| SignatureVerificationError::rejected())
    }
}

/// Validated GitHub webhook event supplied to a business-command mapper.
pub struct GitHubInboundEvent<'a> {
    event_type: &'a str,
    action: &'a str,
    installation_id: GitHubInstallationId,
    repository: &'a GitHubRepositorySlug,
    context: &'a InboundNormalizationContext,
    payload: &'a Value,
}

impl GitHubInboundEvent<'_> {
    #[must_use]
    pub fn event_type(&self) -> &str {
        self.event_type
    }
    #[must_use]
    pub fn action(&self) -> &str {
        self.action
    }
    #[must_use]
    pub const fn installation_id(&self) -> GitHubInstallationId {
        self.installation_id
    }
    #[must_use]
    pub const fn repository(&self) -> &GitHubRepositorySlug {
        self.repository
    }
    #[must_use]
    pub const fn context(&self) -> &InboundNormalizationContext {
        self.context
    }
    #[must_use]
    pub const fn payload(&self) -> &Value {
        self.payload
    }
}

/// Control Plane seam that maps a validated provider event to one formal command.
pub trait GitHubEventMapperPort {
    /// Maps one validated GitHub event to a canonical formal command payload.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe protocol/business mapping failure.
    fn map_event(
        &mut self,
        authority: &ConnectorAuthority,
        event: &GitHubInboundEvent<'_>,
    ) -> Result<NormalizedInboundEvent, ConnectorCallError>;
}

/// GitHub protocol mapper and retry-stable outbound REST adapter.
pub struct GitHubEnterpriseConnector<Credentials, Mapper, Clock> {
    config: GitHubConnectorConfig,
    credentials: Credentials,
    mapper: Mapper,
    clock: Clock,
    agent: ureq::Agent,
}

impl<Credentials, Mapper, Clock> GitHubEnterpriseConnector<Credentials, Mapper, Clock> {
    /// Builds a no-proxy, no-redirect, rustls-verified GitHub connector.
    ///
    /// # Errors
    ///
    /// Rejects malformed explicit TLS roots.
    pub fn try_new(
        config: GitHubConnectorConfig,
        credentials: Credentials,
        mapper: Mapper,
        clock: Clock,
    ) -> Result<Self, IntegrationError> {
        let roots = match &config.tls_roots {
            GitHubTlsRoots::WebPki => ureq::tls::RootCerts::WebPki,
            GitHubTlsRoots::Specific(values) => values
                .iter()
                .map(|value| ureq::tls::Certificate::from_der(value).to_owned())
                .collect::<Vec<_>>()
                .into(),
        };
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .root_certs(roots)
            .use_sni(true)
            .disable_verification(false)
            .build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .timeout_global(Some(config.request_timeout))
            .tls_config(tls)
            .build()
            .into();
        Ok(Self {
            config,
            credentials,
            mapper,
            clock,
            agent,
        })
    }

    #[must_use]
    pub fn into_parts(self) -> (Credentials, Mapper, Clock) {
        (self.credentials, self.mapper, self.clock)
    }
}

impl<Credentials: GitHubCredentialPort, Mapper: GitHubEventMapperPort, Clock: GitHubClock>
    ConnectorPort for GitHubEnterpriseConnector<Credentials, Mapper, Clock>
{
    fn normalize_inbound(
        &mut self,
        authority: &ConnectorAuthority,
        context: &InboundNormalizationContext,
        payload: &[u8],
    ) -> Result<NormalizedInboundEvent, ConnectorCallError> {
        require_authority(&self.config, authority)?;
        let value: Value = serde_json::from_slice(payload).map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "GITHUB_PAYLOAD_INVALID")
        })?;
        validate_webhook_scope(&self.config, &value).map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "GITHUB_SCOPE_MISMATCH")
        })?;
        let action = value.get("action").and_then(Value::as_str).ok_or_else(|| {
            connector_error(ConnectorCallErrorKind::Permanent, "GITHUB_ACTION_INVALID")
        })?;
        let event = GitHubInboundEvent {
            event_type: context.event_type(),
            action,
            installation_id: self.config.installation_id,
            repository: &self.config.repository,
            context,
            payload: &value,
        };
        self.mapper.map_event(authority, &event)
    }

    fn deliver_outbound(
        &mut self,
        claim: &OutboundClaim,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        require_authority(&self.config, claim.authority())?;
        let token = self
            .credentials
            .resolve_installation_token(
                claim.authority().credential_reference_id(),
                self.config.app_id,
                self.config.installation_id,
            )
            .map_err(connector_credential_error)?;
        validate_token(&self.config, &token, self.clock.now_millis())?;
        let operation = GitHubOutboundOperation::parse(claim.operation_name(), claim.payload())?;
        require_permissions(token.permissions, operation.required_permissions())?;
        self.deliver_operation(claim, &token, &operation)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueCommentOperation {
    issue_number: u64,
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PullRequestReviewOperation {
    pull_number: u64,
    body: String,
    event: String,
    commit_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckRunOperation {
    name: String,
    head_sha: String,
    status: String,
    conclusion: Option<String>,
    title: String,
    summary: String,
    details_url: Option<String>,
}

enum GitHubOutboundOperation {
    IssueComment(IssueCommentOperation),
    PullRequestReview(PullRequestReviewOperation),
    CheckRun(CheckRunOperation),
}

impl GitHubOutboundOperation {
    fn parse(name: &str, payload: &[u8]) -> Result<Self, ConnectorCallError> {
        let value = match name {
            "github.issue.comment.v1" => serde_json::from_slice(payload).map(Self::IssueComment),
            "github.pull_request.review.v1" => {
                serde_json::from_slice(payload).map(Self::PullRequestReview)
            }
            "github.check_run.upsert.v1" => serde_json::from_slice(payload).map(Self::CheckRun),
            _ => {
                return Err(connector_error(
                    ConnectorCallErrorKind::Permanent,
                    "GITHUB_OPERATION_UNSUPPORTED",
                ));
            }
        }
        .map_err(|_| {
            connector_error(
                ConnectorCallErrorKind::Permanent,
                "GITHUB_OPERATION_INVALID",
            )
        })?;
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ConnectorCallError> {
        let valid = match self {
            Self::IssueComment(value) => {
                valid_number(value.issue_number) && valid_body(&value.body, 65_536)
            }
            Self::PullRequestReview(value) => {
                valid_number(value.pull_number)
                    && valid_body(&value.body, 65_536)
                    && matches!(
                        value.event.as_str(),
                        "APPROVE" | "REQUEST_CHANGES" | "COMMENT"
                    )
                    && valid_sha(&value.commit_id)
            }
            Self::CheckRun(value) => {
                valid_text(&value.name, 100)
                    && valid_sha(&value.head_sha)
                    && matches!(
                        value.status.as_str(),
                        "queued" | "in_progress" | "completed"
                    )
                    && valid_conclusion(value.status.as_str(), value.conclusion.as_deref())
                    && valid_text(&value.title, 255)
                    && valid_body(&value.summary, 65_535)
                    && value.details_url.as_deref().is_none_or(valid_https_url)
            }
        };
        if valid {
            Ok(())
        } else {
            Err(connector_error(
                ConnectorCallErrorKind::Permanent,
                "GITHUB_OPERATION_INVALID",
            ))
        }
    }

    const fn required_permissions(&self) -> GitHubInstallationPermissions {
        match self {
            Self::IssueComment(_) => GitHubInstallationPermissions::new(
                GitHubPermission::Write,
                GitHubPermission::None,
                GitHubPermission::None,
                GitHubPermission::None,
            ),
            Self::PullRequestReview(_) => GitHubInstallationPermissions::new(
                GitHubPermission::None,
                GitHubPermission::Write,
                GitHubPermission::None,
                GitHubPermission::None,
            ),
            Self::CheckRun(_) => GitHubInstallationPermissions::new(
                GitHubPermission::None,
                GitHubPermission::None,
                GitHubPermission::Write,
                GitHubPermission::Read,
            ),
        }
    }
}

struct GitHubResponse {
    status: u16,
    retry_after_seconds: Option<u64>,
    rate_limited: bool,
    body: Option<Value>,
}

impl<Credentials, Mapper, Clock> GitHubEnterpriseConnector<Credentials, Mapper, Clock> {
    fn deliver_operation(
        &self,
        claim: &OutboundClaim,
        token: &GitHubInstallationToken,
        operation: &GitHubOutboundOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        match operation {
            GitHubOutboundOperation::IssueComment(value) => {
                self.deliver_issue_comment(claim, token, value)
            }
            GitHubOutboundOperation::PullRequestReview(value) => {
                self.deliver_pull_request_review(claim, token, value)
            }
            GitHubOutboundOperation::CheckRun(value) => self.deliver_check_run(claim, token, value),
        }
    }

    fn deliver_issue_comment(
        &self,
        claim: &OutboundClaim,
        token: &GitHubInstallationToken,
        operation: &IssueCommentOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        let marker = operation_marker(claim);
        let path = format!(
            "repos/{}/issues/{}/comments",
            encode_repository(&self.config.repository.0),
            operation.issue_number
        );
        if let Some(remote_id) = self.lookup_marked_array(token, &path, "body", &marker)? {
            return remote_receipt("issue-comment", &remote_id, false);
        }
        let response = self.request(
            token,
            "POST",
            &path,
            Some(&json!({"body": format!("{}\n\n{marker}", operation.body)})),
            Some(claim.operation_key().digest()),
        )?;
        require_success(&response, &[201])?;
        remote_receipt("issue-comment", &response_identity(&response)?, true)
    }

    fn deliver_pull_request_review(
        &self,
        claim: &OutboundClaim,
        token: &GitHubInstallationToken,
        operation: &PullRequestReviewOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        let marker = operation_marker(claim);
        let path = format!(
            "repos/{}/pulls/{}/reviews",
            encode_repository(&self.config.repository.0),
            operation.pull_number
        );
        if let Some(remote_id) = self.lookup_marked_array(token, &path, "body", &marker)? {
            return remote_receipt("pull-request-review", &remote_id, false);
        }
        let response = self.request(
            token,
            "POST",
            &path,
            Some(&json!({
                "body": format!("{}\n\n{marker}", operation.body),
                "commit_id": operation.commit_id,
                "event": operation.event,
            })),
            Some(claim.operation_key().digest()),
        )?;
        require_success(&response, &[200])?;
        remote_receipt("pull-request-review", &response_identity(&response)?, true)
    }

    fn deliver_check_run(
        &self,
        claim: &OutboundClaim,
        token: &GitHubInstallationToken,
        operation: &CheckRunOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        let path = format!(
            "repos/{}/commits/{}/check-runs",
            encode_repository(&self.config.repository.0),
            operation.head_sha
        );
        if let Some(remote_id) =
            self.lookup_external_id(token, &path, &claim.operation_key().digest().0)?
        {
            return remote_receipt("check-run", &remote_id, false);
        }
        let mut body = Map::new();
        body.insert("name".to_owned(), Value::String(operation.name.clone()));
        body.insert(
            "head_sha".to_owned(),
            Value::String(operation.head_sha.clone()),
        );
        body.insert(
            "external_id".to_owned(),
            Value::String(claim.operation_key().digest().0.clone()),
        );
        body.insert("status".to_owned(), Value::String(operation.status.clone()));
        if let Some(conclusion) = &operation.conclusion {
            body.insert("conclusion".to_owned(), Value::String(conclusion.clone()));
        }
        if let Some(details_url) = &operation.details_url {
            body.insert("details_url".to_owned(), Value::String(details_url.clone()));
        }
        body.insert(
            "output".to_owned(),
            json!({"title": operation.title, "summary": operation.summary}),
        );
        let create_path = format!(
            "repos/{}/check-runs",
            encode_repository(&self.config.repository.0)
        );
        let response = self.request(
            token,
            "POST",
            &create_path,
            Some(&Value::Object(body)),
            Some(claim.operation_key().digest()),
        )?;
        require_success(&response, &[201])?;
        remote_receipt("check-run", &response_identity(&response)?, true)
    }

    fn lookup_marked_array(
        &self,
        token: &GitHubInstallationToken,
        path: &str,
        body_field: &str,
        marker: &str,
    ) -> Result<Option<String>, ConnectorCallError> {
        self.lookup_pages(token, path, |body| {
            let values = body.as_array()?;
            values.iter().find_map(|value| {
                let contains_marker = value
                    .get(body_field)
                    .and_then(Value::as_str)
                    .is_some_and(|body| body.contains(marker));
                contains_marker
                    .then(|| response_value_identity(value))
                    .flatten()
            })
        })
    }

    fn lookup_external_id(
        &self,
        token: &GitHubInstallationToken,
        path: &str,
        external_id: &str,
    ) -> Result<Option<String>, ConnectorCallError> {
        self.lookup_pages(token, path, |body| {
            body.get("check_runs")
                .and_then(Value::as_array)
                .and_then(|values| {
                    values.iter().find_map(|value| {
                        let matches = value
                            .get("external_id")
                            .and_then(Value::as_str)
                            .is_some_and(|value| value == external_id);
                        matches.then(|| response_value_identity(value)).flatten()
                    })
                })
        })
    }

    fn lookup_pages(
        &self,
        token: &GitHubInstallationToken,
        path: &str,
        find: impl Fn(&Value) -> Option<String>,
    ) -> Result<Option<String>, ConnectorCallError> {
        for page in 1..=self.config.max_lookup_pages {
            let separator = if path.contains('?') { '&' } else { '?' };
            let page_path = format!("{path}{separator}per_page={PAGE_SIZE}&page={page}");
            let response = self.request(token, "GET", &page_path, None, None)?;
            require_success(&response, &[200])?;
            let body = response.body.as_ref().ok_or_else(|| {
                connector_error(ConnectorCallErrorKind::Retryable, "GITHUB_RESPONSE_INVALID")
            })?;
            if let Some(found) = find(body) {
                return Ok(Some(found));
            }
            let count = body
                .as_array()
                .map(Vec::len)
                .or_else(|| {
                    body.get("check_runs")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                })
                .ok_or_else(|| {
                    connector_error(ConnectorCallErrorKind::Retryable, "GITHUB_RESPONSE_INVALID")
                })?;
            if count < PAGE_SIZE {
                return Ok(None);
            }
        }
        Err(connector_error(
            ConnectorCallErrorKind::Retryable,
            "GITHUB_LOOKUP_BOUND_EXCEEDED",
        ))
    }

    fn request(
        &self,
        token: &GitHubInstallationToken,
        method: &str,
        path: &str,
        body: Option<&Value>,
        idempotency_key: Option<&Sha256Digest>,
    ) -> Result<GitHubResponse, ConnectorCallError> {
        let url = format!(
            "{}{}",
            self.config.api_base_url,
            path.trim_start_matches('/')
        );
        let authorization = format!("Bearer {}", token.value());
        let response = match (method, body) {
            ("GET", None) => self
                .agent
                .get(&url)
                .header("Accept", "application/vnd.github+json")
                .header("Authorization", &authorization)
                .header("User-Agent", USER_AGENT)
                .header("X-GitHub-Api-Version", API_VERSION)
                .call(),
            ("POST", Some(body)) => {
                let mut request = self
                    .agent
                    .post(&url)
                    .header("Accept", "application/vnd.github+json")
                    .header("Authorization", &authorization)
                    .header("User-Agent", USER_AGENT)
                    .header("X-GitHub-Api-Version", API_VERSION);
                if let Some(key) = idempotency_key {
                    request = request.header("X-GitHub-Idempotency-Key", &key.0);
                }
                request.send_json(body)
            }
            _ => {
                return Err(connector_error(
                    ConnectorCallErrorKind::Permanent,
                    "GITHUB_REQUEST_INVALID",
                ));
            }
        }
        .map_err(|_| {
            connector_error(
                ConnectorCallErrorKind::Retryable,
                "GITHUB_TRANSPORT_UNAVAILABLE",
            )
        })?;
        let status = response.status().as_u16();
        let retry_after_seconds = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let rate_limited = status == 429
            || status == 403
                && (response
                    .headers()
                    .get("x-ratelimit-remaining")
                    .is_some_and(|value| value == "0")
                    || retry_after_seconds.is_some());
        let bytes = response
            .into_body()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(|_| {
                connector_error(
                    ConnectorCallErrorKind::Retryable,
                    "GITHUB_RESPONSE_UNREADABLE",
                )
            })?;
        let body = if bytes.is_empty() {
            None
        } else {
            Some(serde_json::from_slice(&bytes).map_err(|_| {
                connector_error(ConnectorCallErrorKind::Retryable, "GITHUB_RESPONSE_INVALID")
            })?)
        };
        Ok(GitHubResponse {
            status,
            retry_after_seconds,
            rate_limited,
            body,
        })
    }
}

fn require_success(response: &GitHubResponse, accepted: &[u16]) -> Result<(), ConnectorCallError> {
    if accepted.contains(&response.status) {
        return Ok(());
    }
    if response.rate_limited {
        return Err(rate_limit_error(response.retry_after_seconds));
    }
    let (kind, code) = match response.status {
        401 => (
            ConnectorCallErrorKind::CredentialRevoked,
            "GITHUB_CREDENTIAL_REVOKED",
        ),
        403 => (
            ConnectorCallErrorKind::Permanent,
            "GITHUB_PERMISSION_DENIED",
        ),
        408 | 409 | 425 | 500..=599 => (
            ConnectorCallErrorKind::Retryable,
            "GITHUB_SERVICE_UNAVAILABLE",
        ),
        _ => (ConnectorCallErrorKind::Permanent, "GITHUB_REQUEST_REJECTED"),
    };
    Err(connector_error(kind, code))
}

fn rate_limit_error(retry_after_seconds: Option<u64>) -> ConnectorCallError {
    let hinted = retry_after_seconds
        .and_then(|seconds| seconds.checked_mul(1_000))
        .and_then(|millis| ConnectorCallError::retryable_after("GITHUB_RATE_LIMITED", millis).ok());
    if let Some(error) = hinted {
        error
    } else {
        connector_error(ConnectorCallErrorKind::Retryable, "GITHUB_RATE_LIMITED")
    }
}

fn remote_receipt(
    resource_kind: &str,
    remote_id: &str,
    remote_write_performed: bool,
) -> Result<OutboundCallReceipt, ConnectorCallError> {
    let mut hash = Sha256::new();
    hash.update(b"winwincode.github.remote-receipt.v1");
    hash.update([0]);
    hash.update(resource_kind.as_bytes());
    hash.update([0]);
    hash.update(remote_id.as_bytes());
    OutboundCallReceipt::try_new(
        Sha256Digest(format!("sha256:{:x}", hash.finalize())),
        remote_write_performed,
    )
    .map_err(|_| connector_error(ConnectorCallErrorKind::Permanent, "GITHUB_RESPONSE_INVALID"))
}

fn response_identity(response: &GitHubResponse) -> Result<String, ConnectorCallError> {
    response
        .body
        .as_ref()
        .and_then(response_value_identity)
        .ok_or_else(|| {
            connector_error(ConnectorCallErrorKind::Retryable, "GITHUB_RESPONSE_INVALID")
        })
}

fn response_value_identity(value: &Value) -> Option<String> {
    value
        .get("node_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            value
                .get("id")
                .and_then(Value::as_u64)
                .map(|id| id.to_string())
        })
}

fn operation_marker(claim: &OutboundClaim) -> String {
    format!(
        "<!-- winwincode-integration:{} -->",
        claim.operation_key().digest().0
    )
}

fn require_permissions(
    actual: GitHubInstallationPermissions,
    required: GitHubInstallationPermissions,
) -> Result<(), ConnectorCallError> {
    if actual.issues.permits(required.issues)
        && actual.pull_requests.permits(required.pull_requests)
        && actual.checks.permits(required.checks)
        && actual.contents.permits(required.contents)
    {
        Ok(())
    } else {
        Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "GITHUB_PERMISSION_DENIED",
        ))
    }
}

fn validate_token(
    config: &GitHubConnectorConfig,
    token: &GitHubInstallationToken,
    now_millis: u64,
) -> Result<(), ConnectorCallError> {
    if now_millis == 0 || now_millis > MAX_SAFE_INTEGER {
        return Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "GITHUB_CLOCK_INVALID",
        ));
    }
    if token.app_id != config.app_id
        || token.installation_id != config.installation_id
        || !same_repository(&token.repository.0, &config.repository.0)
    {
        return Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "GITHUB_TOKEN_SCOPE_MISMATCH",
        ));
    }
    if token.expires_at_millis <= now_millis {
        return Err(connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "GITHUB_CREDENTIAL_REVOKED",
        ));
    }
    Ok(())
}

fn require_authority(
    config: &GitHubConnectorConfig,
    authority: &ConnectorAuthority,
) -> Result<(), ConnectorCallError> {
    if matches_authority(config, authority) && authority.state() == crate::ConnectorState::Active {
        Ok(())
    } else if authority.state() == crate::ConnectorState::CredentialRevoked {
        Err(connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "GITHUB_CREDENTIAL_REVOKED",
        ))
    } else {
        Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "GITHUB_AUTHORITY_MISMATCH",
        ))
    }
}

fn matches_authority(config: &GitHubConnectorConfig, authority: &ConnectorAuthority) -> bool {
    authority.integration_id() == &config.integration_id
        && authority.credential_reference_id() == &config.credential_reference_id
        && authority.protocol().as_str() == GITHUB_CONNECTOR_PROTOCOL
}

fn signature_credential_error(error: GitHubCredentialError) -> SignatureVerificationError {
    match error.kind() {
        GitHubCredentialErrorKind::Revoked => SignatureVerificationError::credential_revoked(),
        GitHubCredentialErrorKind::PermissionDenied | GitHubCredentialErrorKind::Unavailable => {
            SignatureVerificationError::rejected()
        }
    }
}

fn connector_credential_error(error: GitHubCredentialError) -> ConnectorCallError {
    match error.kind() {
        GitHubCredentialErrorKind::Revoked => connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "GITHUB_CREDENTIAL_REVOKED",
        ),
        GitHubCredentialErrorKind::PermissionDenied => connector_error(
            ConnectorCallErrorKind::Permanent,
            "GITHUB_PERMISSION_DENIED",
        ),
        GitHubCredentialErrorKind::Unavailable => connector_error(
            ConnectorCallErrorKind::Retryable,
            "GITHUB_CREDENTIAL_UNAVAILABLE",
        ),
    }
}

fn validate_webhook_scope(
    config: &GitHubConnectorConfig,
    value: &Value,
) -> Result<(), IntegrationError> {
    let installation = value
        .get("installation")
        .and_then(|value| value.get("id"))
        .and_then(Value::as_u64);
    let repository = value
        .get("repository")
        .and_then(|value| value.get("full_name"))
        .and_then(Value::as_str);
    if installation == Some(config.installation_id.get())
        && repository.is_some_and(|value| same_repository(value, &config.repository.0))
    {
        Ok(())
    } else {
        Err(invalid("GitHub webhook scope does not match the connector"))
    }
}

fn webhook_ordering(event_type: &str, value: &Value) -> Result<(String, u64), IntegrationError> {
    let (resource, timestamp) = match event_type {
        "issues" => ("issue", timestamp_field(value, "issue", "updated_at")?),
        "pull_request" => (
            "pull_request",
            timestamp_field(value, "pull_request", "updated_at")?,
        ),
        "pull_request_review" => ("review", timestamp_field(value, "review", "submitted_at")?),
        "check_run" => {
            let status = value
                .get("check_run")
                .and_then(|value| value.get("status"))
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("GitHub check-run status is invalid"))?;
            let rank = match status {
                "queued" => 1,
                "in_progress" => 2,
                "completed" => 3,
                _ => return Err(invalid("GitHub check-run status is invalid")),
            };
            let id = resource_id(value, "check_run")?;
            return Ok((format!("check_run:{id}"), rank));
        }
        _ => return Err(invalid("GitHub webhook event is unsupported")),
    };
    let id = resource_id(value, resource)?;
    Ok((format!("{resource}:{id}"), timestamp))
}

fn timestamp_field(value: &Value, resource: &str, field: &str) -> Result<u64, IntegrationError> {
    let timestamp = value
        .get(resource)
        .and_then(|value| value.get(field))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("GitHub webhook timestamp is invalid"))?;
    let value = OffsetDateTime::parse(timestamp, &Rfc3339)
        .map_err(|_| invalid("GitHub webhook timestamp is invalid"))?
        .unix_timestamp_nanos();
    let millis = value
        .checked_div(1_000_000)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| invalid("GitHub webhook timestamp is invalid"))?;
    if millis == 0 || millis > MAX_SAFE_INTEGER {
        return Err(invalid("GitHub webhook timestamp is invalid"));
    }
    Ok(millis)
}

fn resource_id(value: &Value, resource: &str) -> Result<String, IntegrationError> {
    let value = value
        .get(resource)
        .ok_or_else(|| invalid("GitHub webhook resource is invalid"))?;
    value
        .get("node_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            value
                .get("id")
                .and_then(Value::as_u64)
                .map(|id| id.to_string())
        })
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or_else(|| invalid("GitHub webhook resource is invalid"))
}

fn decode_signature(value: &[u8]) -> Option<[u8; 32]> {
    let encoded = value.strip_prefix(b"sha256=")?;
    if encoded.len() != 64 {
        return None;
    }
    let mut output = [0_u8; 32];
    for (index, pair) in encoded.chunks_exact(2).enumerate() {
        output[index] = hex_nibble(pair[0])?.checked_mul(16)? + hex_nibble(pair[1])?;
    }
    Some(output)
}

const fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn canonical_api_base_url(value: &str) -> Result<String, IntegrationError> {
    let uri =
        ureq::http::Uri::from_str(value).map_err(|_| invalid("GitHub API base URL is invalid"))?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| invalid("GitHub API base URL is invalid"))?;
    let authority = uri
        .authority()
        .ok_or_else(|| invalid("GitHub API base URL is invalid"))?;
    if scheme != "https"
        || authority.as_str().contains('@')
        || uri
            .path_and_query()
            .and_then(|value| value.query())
            .is_some()
    {
        return Err(invalid("GitHub API base URL must be credential-free HTTPS"));
    }
    let mut canonical = value.trim_end_matches('/').to_owned();
    canonical.push('/');
    Ok(canonical)
}

fn validate_tls_roots(value: &GitHubTlsRoots) -> Result<(), IntegrationError> {
    if let GitHubTlsRoots::Specific(values) = value
        && (values.is_empty()
            || values.len() > 32
            || values
                .iter()
                .any(|value| value.is_empty() || value.len() > 64 * 1_024))
    {
        return Err(invalid("GitHub TLS roots are invalid"));
    }
    Ok(())
}

fn validate_prefixed_id(value: &str, prefix: &str) -> Result<(), IntegrationError> {
    const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let valid = value
        .strip_prefix(&format!("{prefix}_"))
        .is_some_and(|suffix| {
            suffix.len() == 26 && suffix.bytes().all(|byte| CROCKFORD.contains(&byte))
        });
    if valid {
        Ok(())
    } else {
        Err(invalid("GitHub credential reference is invalid"))
    }
}

fn validate_repository(value: &str) -> Result<(), IntegrationError> {
    let mut segments = value.split('/');
    let owner = segments.next();
    let repository = segments.next();
    let valid = segments.next().is_none()
        && owner.is_some_and(valid_repository_segment)
        && repository.is_some_and(valid_repository_segment);
    if valid {
        Ok(())
    } else {
        Err(invalid("GitHub repository is invalid"))
    }
}

fn valid_repository_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && value != "."
        && value != ".."
}

fn encode_repository(value: &str) -> String {
    value
        .split('/')
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_path_segment(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut output, "%{byte:02X}").expect("writing into String cannot fail");
        }
    }
    output
}

fn same_repository(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn valid_number(value: u64) -> bool {
    value > 0 && value <= MAX_SAFE_INTEGER
}

fn valid_text(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn valid_body(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.contains('\0')
}

fn valid_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_conclusion(status: &str, conclusion: Option<&str>) -> bool {
    if status == "completed" {
        conclusion.is_some_and(|value| {
            matches!(
                value,
                "action_required"
                    | "cancelled"
                    | "failure"
                    | "neutral"
                    | "skipped"
                    | "stale"
                    | "success"
                    | "timed_out"
            )
        })
    } else {
        conclusion.is_none()
    }
}

fn valid_https_url(value: &str) -> bool {
    ureq::http::Uri::from_str(value).is_ok_and(|uri| {
        uri.scheme_str() == Some("https")
            && uri.authority().is_some()
            && !uri
                .authority()
                .is_some_and(|value| value.as_str().contains('@'))
    })
}

fn connector_error(kind: ConnectorCallErrorKind, code: &str) -> ConnectorCallError {
    ConnectorCallError::try_new(kind, code).expect("GitHub error codes are portable")
}

fn invalid(message: &'static str) -> IntegrationError {
    IntegrationError::new(IntegrationErrorKind::Invalid, message)
}
