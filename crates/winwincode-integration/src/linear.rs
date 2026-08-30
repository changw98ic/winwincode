// SPDX-License-Identifier: Apache-2.0

//! Linear OAuth and GraphQL adapter over the durable Integration Framework.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use winwincode_audit::AuditScope;
use winwincode_domain::{CredentialReferenceId, EnterpriseIntegrationId, Sha256Digest};

use crate::model::{MAX_SAFE_INTEGER, validate_integration_id};
use crate::{
    ConnectorAuthority, ConnectorCallError, ConnectorCallErrorKind, ConnectorPort,
    InboundNormalizationContext, InboundWebhookMetadata, InboundWebhookRequest, IntegrationError,
    IntegrationErrorKind, NormalizedInboundEvent, OutboundCallReceipt, OutboundClaim,
    SignatureVerificationError, WebhookSignatureVerifier,
};

/// Canonical Integration Framework protocol identifier for Linear OAuth apps.
pub const LINEAR_CONNECTOR_PROTOCOL: &str = "linear.oauth.v1";
const USER_AGENT: &str = "WinWinCode-Linear-Enterprise-Connector";
const MAX_RESPONSE_BYTES: u64 = 2 * 1_024 * 1_024;
const WEBHOOK_REPLAY_WINDOW_MILLIS: u64 = 60_000;

macro_rules! linear_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            /// Builds one canonical Linear UUID.
            ///
            /// # Errors
            ///
            /// Rejects a non-canonical lowercase UUID.
            pub fn try_new(value: impl Into<String>) -> Result<Self, IntegrationError> {
                let value = value.into();
                validate_uuid(&value)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

linear_id!(LinearWorkspaceId, "Canonical Linear workspace identity.");
linear_id!(LinearTeamId, "Canonical Linear team identity.");
linear_id!(LinearProjectId, "Canonical Linear project identity.");
linear_id!(LinearIssueId, "Canonical Linear issue identity.");
linear_id!(LinearCommentId, "Canonical Linear comment identity.");

/// Exact Linear workspace/team and optional project boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinearConnectorScope {
    workspace: LinearWorkspaceId,
    team: LinearTeamId,
    project: Option<LinearProjectId>,
}

impl LinearConnectorScope {
    #[must_use]
    pub const fn new(
        workspace_id: LinearWorkspaceId,
        team_id: LinearTeamId,
        project_id: Option<LinearProjectId>,
    ) -> Self {
        Self {
            workspace: workspace_id,
            team: team_id,
            project: project_id,
        }
    }

    #[must_use]
    pub const fn workspace_id(&self) -> &LinearWorkspaceId {
        &self.workspace
    }

    #[must_use]
    pub const fn team_id(&self) -> &LinearTeamId {
        &self.team
    }

    #[must_use]
    pub const fn project_id(&self) -> Option<&LinearProjectId> {
        self.project.as_ref()
    }
}

/// Explicit TLS roots for Linear's GraphQL endpoint or a local fixture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LinearTlsRoots {
    WebPki,
    Specific(Vec<Vec<u8>>),
}

/// Credential-free Linear connector configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinearConnectorConfig {
    integration_id: EnterpriseIntegrationId,
    credential_reference_id: CredentialReferenceId,
    scope: LinearConnectorScope,
    graphql_endpoint: String,
    tls_roots: LinearTlsRoots,
    request_timeout: Duration,
    max_lookup_pages: u16,
}

impl LinearConnectorConfig {
    /// Builds an exact Linear OAuth/GraphQL boundary.
    ///
    /// # Errors
    ///
    /// Rejects invalid authority IDs, endpoints, or TLS roots.
    pub fn try_new(
        integration_id: EnterpriseIntegrationId,
        credential_reference_id: CredentialReferenceId,
        scope: LinearConnectorScope,
        graphql_endpoint: impl Into<String>,
        tls_roots: LinearTlsRoots,
    ) -> Result<Self, IntegrationError> {
        validate_integration_id(&integration_id)?;
        validate_prefixed_id(&credential_reference_id.0, "crd")?;
        validate_tls_roots(&tls_roots)?;
        let graphql_endpoint = canonical_graphql_endpoint(&graphql_endpoint.into())?;
        Ok(Self {
            integration_id,
            credential_reference_id,
            scope,
            graphql_endpoint,
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
    pub const fn scope(&self) -> &LinearConnectorScope {
        &self.scope
    }
}

/// Closed Linear OAuth scope set used by the adapter.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LinearOAuthScope {
    Read,
    Write,
    IssuesCreate,
    CommentsCreate,
}

/// Stable secret-resolution failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinearCredentialErrorKind {
    Revoked,
    PermissionDenied,
    Unavailable,
}

/// Secret-safe Linear credential resolution error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinearCredentialError {
    kind: LinearCredentialErrorKind,
}

impl LinearCredentialError {
    #[must_use]
    pub const fn new(kind: LinearCredentialErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(self) -> LinearCredentialErrorKind {
        self.kind
    }
}

impl fmt::Display for LinearCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Linear credential could not be resolved")
    }
}

impl std::error::Error for LinearCredentialError {}

/// Short-lived Linear webhook secret that is zeroed on drop.
pub struct LinearWebhookSecret(Vec<u8>);

impl LinearWebhookSecret {
    /// Builds an opaque webhook secret.
    ///
    /// # Errors
    ///
    /// Rejects empty or oversized secret material.
    pub fn try_new(value: impl AsRef<[u8]>) -> Result<Self, IntegrationError> {
        let value = value.as_ref();
        if value.is_empty() || value.len() > 4_096 {
            return Err(invalid("Linear webhook secret is invalid"));
        }
        Ok(Self(value.to_vec()))
    }

    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for LinearWebhookSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LinearWebhookSecret([REDACTED])")
    }
}

impl Drop for LinearWebhookSecret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Short-lived OAuth access token and its credential-owner scope attestation.
pub struct LinearOAuthToken {
    token: Vec<u8>,
    workspace_id: LinearWorkspaceId,
    scopes: BTreeSet<LinearOAuthScope>,
    expires_at_millis: u64,
}

impl LinearOAuthToken {
    /// Builds a short-lived access token returned by the credential authority.
    ///
    /// # Errors
    ///
    /// Rejects invalid token bytes, an empty scope set, or invalid expiry.
    pub fn try_new(
        token: impl AsRef<[u8]>,
        workspace_id: LinearWorkspaceId,
        scopes: impl IntoIterator<Item = LinearOAuthScope>,
        expires_at_millis: u64,
    ) -> Result<Self, IntegrationError> {
        let token = token.as_ref();
        let scopes = scopes.into_iter().collect::<BTreeSet<_>>();
        if token.is_empty()
            || token.len() > 4_096
            || !token.iter().all(|byte| matches!(byte, 0x21..=0x7e))
            || scopes.is_empty()
            || expires_at_millis == 0
            || expires_at_millis > MAX_SAFE_INTEGER
        {
            return Err(invalid("Linear OAuth token is invalid"));
        }
        Ok(Self {
            token: token.to_vec(),
            workspace_id,
            scopes,
            expires_at_millis,
        })
    }

    fn value(&self) -> &str {
        std::str::from_utf8(&self.token).expect("validated visible ASCII token")
    }

    fn permits(&self, required: LinearOAuthScope) -> bool {
        self.scopes.contains(&LinearOAuthScope::Write)
            || self.scopes.contains(&required)
            || required == LinearOAuthScope::Read && self.scopes.contains(&LinearOAuthScope::Read)
    }
}

impl fmt::Debug for LinearOAuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LinearOAuthToken")
            .field("workspace_id", &self.workspace_id)
            .field("scopes", &self.scopes)
            .field("expires_at_millis", &self.expires_at_millis)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl Drop for LinearOAuthToken {
    fn drop(&mut self) {
        self.token.fill(0);
    }
}

/// Credential-owner port. Refresh tokens and client secrets stay behind this seam.
pub trait LinearCredentialPort {
    /// Resolves the webhook secret for one credential reference.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe credential error.
    fn resolve_webhook_secret(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<LinearWebhookSecret, LinearCredentialError>;

    /// Resolves or refreshes a short-lived OAuth token for one workspace.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe credential error.
    fn resolve_oauth_token(
        &mut self,
        reference: &CredentialReferenceId,
        workspace_id: &LinearWorkspaceId,
    ) -> Result<LinearOAuthToken, LinearCredentialError>;
}

/// Time authority used for OAuth expiry, webhook replay, and rate-limit reset checks.
pub trait LinearClock {
    fn now_millis(&self) -> u64;
}

/// Closed Linear webhook resource type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinearEventKind {
    Issue,
    Comment,
    Project,
    OAuthApp,
}

impl LinearEventKind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "Issue" => Some(Self::Issue),
            "Comment" => Some(Self::Comment),
            "Project" => Some(Self::Project),
            "OAuthApp" => Some(Self::OAuthApp),
            _ => None,
        }
    }

    const fn canonical_name(self) -> &'static str {
        match self {
            Self::Issue => "linear.issue",
            Self::Comment => "linear.comment",
            Self::Project => "linear.project",
            Self::OAuthApp => "linear.oauth_app",
        }
    }
}

/// Closed Linear webhook action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinearEventAction {
    Create,
    Update,
    Remove,
    Revoked,
}

impl LinearEventAction {
    fn parse(kind: LinearEventKind, value: &str) -> Option<Self> {
        match (kind, value) {
            (LinearEventKind::OAuthApp, "revoked") => Some(Self::Revoked),
            (
                LinearEventKind::Issue | LinearEventKind::Comment | LinearEventKind::Project,
                "create",
            ) => Some(Self::Create),
            (
                LinearEventKind::Issue | LinearEventKind::Comment | LinearEventKind::Project,
                "update",
            ) => Some(Self::Update),
            (
                LinearEventKind::Issue | LinearEventKind::Comment | LinearEventKind::Project,
                "remove",
            ) => Some(Self::Remove),
            _ => None,
        }
    }
}

/// Exact Linear webhook headers before raw-body authentication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinearWebhookHeaders {
    delivery_id: String,
    event_kind: LinearEventKind,
    signature: Vec<u8>,
    timestamp_millis: u64,
}

impl LinearWebhookHeaders {
    /// Builds the four canonical `Linear-*` webhook headers.
    ///
    /// # Errors
    ///
    /// Rejects invalid UUID, resource type, signature, or timestamp headers.
    pub fn try_new(
        delivery_id: impl Into<String>,
        event_type: impl AsRef<str>,
        signature: impl AsRef<[u8]>,
        timestamp_millis: u64,
    ) -> Result<Self, IntegrationError> {
        let delivery_id = delivery_id.into();
        validate_uuid(&delivery_id)?;
        let event_kind = LinearEventKind::parse(event_type.as_ref())
            .ok_or_else(|| invalid("Linear webhook event type is invalid"))?;
        let signature = signature.as_ref();
        if decode_signature(signature).is_none()
            || timestamp_millis == 0
            || timestamp_millis > MAX_SAFE_INTEGER
        {
            return Err(invalid("Linear webhook headers are invalid"));
        }
        Ok(Self {
            delivery_id,
            event_kind,
            signature: signature.to_vec(),
            timestamp_millis,
        })
    }
}

/// Builds Integration Framework requests from Linear webhook headers and raw bodies.
#[derive(Clone, Debug)]
pub struct LinearWebhookRequestFactory {
    config: LinearConnectorConfig,
}

impl LinearWebhookRequestFactory {
    #[must_use]
    pub const fn new(config: LinearConnectorConfig) -> Self {
        Self { config }
    }

    /// Builds one authenticated-input request with resource-local ordering.
    ///
    /// # Errors
    ///
    /// Rejects unsupported payloads, mismatched headers, or foreign workspace scope.
    pub fn build(
        &self,
        scope: AuditScope,
        headers: LinearWebhookHeaders,
        payload: Vec<u8>,
        received_at_millis: u64,
    ) -> Result<InboundWebhookRequest, IntegrationError> {
        let value: Value = serde_json::from_slice(&payload)
            .map_err(|_| invalid("Linear webhook payload is invalid"))?;
        let envelope = parse_webhook_value(&self.config, &value)?;
        if envelope.kind != headers.event_kind
            || envelope.timestamp_millis != headers.timestamp_millis
        {
            return Err(invalid("Linear webhook headers do not match the body"));
        }
        let metadata = InboundWebhookMetadata::try_new(
            envelope.kind.canonical_name(),
            headers.delivery_id,
            envelope.ordering_key,
            envelope.timestamp_millis,
            received_at_millis,
        )?;
        InboundWebhookRequest::try_new(
            self.config.integration_id.clone(),
            scope,
            metadata,
            headers.signature,
            payload,
        )
    }
}

/// HMAC-SHA256 and one-minute replay verifier for exact raw Linear bodies.
pub struct LinearWebhookVerifier<Credentials, Clock> {
    config: LinearConnectorConfig,
    credentials: Credentials,
    clock: Clock,
}

impl<Credentials, Clock> LinearWebhookVerifier<Credentials, Clock> {
    #[must_use]
    pub const fn new(
        config: LinearConnectorConfig,
        credentials: Credentials,
        clock: Clock,
    ) -> Self {
        Self {
            config,
            credentials,
            clock,
        }
    }

    #[must_use]
    pub fn into_parts(self) -> (Credentials, Clock) {
        (self.credentials, self.clock)
    }
}

impl<Credentials: LinearCredentialPort, Clock: LinearClock> WebhookSignatureVerifier
    for LinearWebhookVerifier<Credentials, Clock>
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
            .map_err(|_| SignatureVerificationError::rejected())?;
        let value: Value =
            serde_json::from_slice(payload).map_err(|_| SignatureVerificationError::rejected())?;
        let envelope = parse_webhook_value(&self.config, &value)
            .map_err(|_| SignatureVerificationError::rejected())?;
        let now = self.clock.now_millis();
        if now == 0
            || now > MAX_SAFE_INTEGER
            || now.abs_diff(envelope.timestamp_millis) > WEBHOOK_REPLAY_WINDOW_MILLIS
        {
            return Err(SignatureVerificationError::rejected());
        }
        if envelope.action == LinearEventAction::Revoked {
            return Err(SignatureVerificationError::credential_revoked());
        }
        Ok(())
    }
}

/// Validated Linear webhook event supplied to a business-command mapper.
pub struct LinearInboundEvent<'a> {
    kind: LinearEventKind,
    action: LinearEventAction,
    resource_id: &'a str,
    issue_id: Option<&'a str>,
    scope: &'a LinearConnectorScope,
    context: &'a InboundNormalizationContext,
    payload: &'a Value,
}

impl LinearInboundEvent<'_> {
    #[must_use]
    pub const fn kind(&self) -> LinearEventKind {
        self.kind
    }
    #[must_use]
    pub const fn action(&self) -> LinearEventAction {
        self.action
    }
    #[must_use]
    pub fn resource_id(&self) -> &str {
        self.resource_id
    }
    #[must_use]
    pub const fn issue_id(&self) -> Option<&str> {
        self.issue_id
    }
    #[must_use]
    pub const fn scope(&self) -> &LinearConnectorScope {
        self.scope
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

/// Control Plane seam mapping one validated Linear event to a formal command.
pub trait LinearEventMapperPort {
    /// Maps one validated provider event to canonical command JSON.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe mapping failure.
    fn map_event(
        &mut self,
        authority: &ConnectorAuthority,
        event: &LinearInboundEvent<'_>,
    ) -> Result<NormalizedInboundEvent, ConnectorCallError>;
}

/// Linear protocol mapper and retry-stable outbound GraphQL adapter.
pub struct LinearEnterpriseConnector<Credentials, Mapper, Clock> {
    config: LinearConnectorConfig,
    credentials: Credentials,
    mapper: Mapper,
    clock: Clock,
    agent: ureq::Agent,
}

impl<Credentials, Mapper, Clock> LinearEnterpriseConnector<Credentials, Mapper, Clock> {
    /// Builds a no-proxy, no-redirect, rustls-verified Linear connector.
    ///
    /// # Errors
    ///
    /// Rejects malformed explicit TLS roots.
    pub fn try_new(
        config: LinearConnectorConfig,
        credentials: Credentials,
        mapper: Mapper,
        clock: Clock,
    ) -> Result<Self, IntegrationError> {
        let roots = match &config.tls_roots {
            LinearTlsRoots::WebPki => ureq::tls::RootCerts::WebPki,
            LinearTlsRoots::Specific(values) => values
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

impl<Credentials: LinearCredentialPort, Mapper: LinearEventMapperPort, Clock: LinearClock>
    ConnectorPort for LinearEnterpriseConnector<Credentials, Mapper, Clock>
{
    fn normalize_inbound(
        &mut self,
        authority: &ConnectorAuthority,
        context: &InboundNormalizationContext,
        payload: &[u8],
    ) -> Result<NormalizedInboundEvent, ConnectorCallError> {
        require_authority(&self.config, authority)?;
        let value: Value = serde_json::from_slice(payload).map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "LINEAR_PAYLOAD_INVALID")
        })?;
        let envelope = parse_webhook_value(&self.config, &value).map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "LINEAR_SCOPE_MISMATCH")
        })?;
        if envelope.kind.canonical_name() != context.event_type()
            || envelope.action == LinearEventAction::Revoked
        {
            return Err(connector_error(
                ConnectorCallErrorKind::Permanent,
                "LINEAR_EVENT_MISMATCH",
            ));
        }
        let event = LinearInboundEvent {
            kind: envelope.kind,
            action: envelope.action,
            resource_id: envelope.resource_id,
            issue_id: envelope.issue_id,
            scope: &self.config.scope,
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
        let operation = LinearOutboundOperation::parse(claim.operation_name(), claim.payload())?;
        operation.require_scope(&self.config.scope)?;
        let token = self
            .credentials
            .resolve_oauth_token(
                claim.authority().credential_reference_id(),
                self.config.scope.workspace_id(),
            )
            .map_err(connector_credential_error)?;
        validate_token(&self.config, &token, self.clock.now_millis())?;
        require_oauth_scope(&token, operation.required_scope())?;
        self.deliver_operation(claim, &token, &operation)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueCreateOperation {
    title: String,
    description: String,
    state_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueStatusOperation {
    #[serde(rename = "issue_id")]
    issue: String,
    #[serde(rename = "state_id")]
    state: String,
    #[serde(rename = "team_id")]
    team: String,
    #[serde(rename = "project_id")]
    project: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentCreateOperation {
    #[serde(rename = "issue_id")]
    issue: String,
    body: String,
    #[serde(rename = "team_id")]
    team: String,
    #[serde(rename = "project_id")]
    project: Option<String>,
}

enum LinearOutboundOperation {
    IssueCreate(IssueCreateOperation),
    IssueStatus(IssueStatusOperation),
    CommentCreate(CommentCreateOperation),
}

impl LinearOutboundOperation {
    fn parse(name: &str, payload: &[u8]) -> Result<Self, ConnectorCallError> {
        let parsed = match name {
            "linear.issue.create.v1" => serde_json::from_slice(payload).map(Self::IssueCreate),
            "linear.issue.status.set.v1" => serde_json::from_slice(payload).map(Self::IssueStatus),
            "linear.comment.create.v1" => serde_json::from_slice(payload).map(Self::CommentCreate),
            _ => {
                return Err(connector_error(
                    ConnectorCallErrorKind::Permanent,
                    "LINEAR_OPERATION_UNSUPPORTED",
                ));
            }
        }
        .map_err(|_| {
            connector_error(
                ConnectorCallErrorKind::Permanent,
                "LINEAR_OPERATION_INVALID",
            )
        })?;
        parsed.validate()?;
        Ok(parsed)
    }

    fn validate(&self) -> Result<(), ConnectorCallError> {
        let valid = match self {
            Self::IssueCreate(value) => {
                valid_text(&value.title, 255)
                    && valid_body(&value.description, 65_536)
                    && value.state_id.as_deref().is_none_or(valid_linear_uuid)
            }
            Self::IssueStatus(value) => {
                valid_linear_uuid(&value.issue)
                    && valid_linear_uuid(&value.state)
                    && valid_linear_uuid(&value.team)
                    && value.project.as_deref().is_none_or(valid_linear_uuid)
            }
            Self::CommentCreate(value) => {
                valid_linear_uuid(&value.issue)
                    && valid_body(&value.body, 65_536)
                    && valid_linear_uuid(&value.team)
                    && value.project.as_deref().is_none_or(valid_linear_uuid)
            }
        };
        if valid {
            Ok(())
        } else {
            Err(connector_error(
                ConnectorCallErrorKind::Permanent,
                "LINEAR_OPERATION_INVALID",
            ))
        }
    }

    fn require_scope(&self, scope: &LinearConnectorScope) -> Result<(), ConnectorCallError> {
        let matches = match self {
            Self::IssueCreate(_) => true,
            Self::IssueStatus(value) => {
                value.team == scope.team_id().as_str()
                    && option_matches(value.project.as_deref(), scope.project_id())
            }
            Self::CommentCreate(value) => {
                value.team == scope.team_id().as_str()
                    && option_matches(value.project.as_deref(), scope.project_id())
            }
        };
        if matches {
            Ok(())
        } else {
            Err(connector_error(
                ConnectorCallErrorKind::Permanent,
                "LINEAR_SCOPE_MISMATCH",
            ))
        }
    }

    const fn required_scope(&self) -> LinearOAuthScope {
        match self {
            Self::IssueCreate(_) => LinearOAuthScope::IssuesCreate,
            Self::IssueStatus(_) => LinearOAuthScope::Write,
            Self::CommentCreate(_) => LinearOAuthScope::CommentsCreate,
        }
    }
}

struct LinearResponse {
    status: u16,
    reset_at_millis: Option<u64>,
    body: Option<Value>,
}

impl<Credentials, Mapper, Clock: LinearClock>
    LinearEnterpriseConnector<Credentials, Mapper, Clock>
{
    fn deliver_operation(
        &self,
        claim: &OutboundClaim,
        token: &LinearOAuthToken,
        operation: &LinearOutboundOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        match operation {
            LinearOutboundOperation::IssueCreate(value) => {
                self.deliver_issue_create(claim, token, value)
            }
            LinearOutboundOperation::IssueStatus(value) => self.deliver_issue_status(token, value),
            LinearOutboundOperation::CommentCreate(value) => {
                self.deliver_comment_create(claim, token, value)
            }
        }
    }

    fn deliver_issue_create(
        &self,
        claim: &OutboundClaim,
        token: &LinearOAuthToken,
        operation: &IssueCreateOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        let marker = operation_marker(claim);
        if let Some(remote_id) = self.lookup_issue_marker(token, &marker)? {
            return remote_receipt("issue", &remote_id, false);
        }
        let mut input = Map::new();
        input.insert("teamId".to_owned(), json!(self.config.scope.team.as_str()));
        input.insert("title".to_owned(), json!(operation.title));
        input.insert(
            "description".to_owned(),
            json!(format!("{}\n\n{marker}", operation.description)),
        );
        if let Some(project_id) = self.config.scope.project_id() {
            input.insert("projectId".to_owned(), json!(project_id.as_str()));
        }
        if let Some(state_id) = &operation.state_id {
            input.insert("stateId".to_owned(), json!(state_id));
        }
        let response = self.graphql(
            token,
            "mutation WinWinCodeIssueCreate($input: IssueCreateInput!) { issueCreate(input: $input) { success issue { id } } }",
            &json!({"input": input}),
        )?;
        let data = require_graphql_success(&response, self.clock.now_millis())?;
        let result = data.get("issueCreate").ok_or_else(response_invalid)?;
        require_mutation_success(result)?;
        let id = result
            .get("issue")
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .filter(|id| valid_linear_uuid(id))
            .ok_or_else(response_invalid)?;
        remote_receipt("issue", id, true)
    }

    fn deliver_issue_status(
        &self,
        token: &LinearOAuthToken,
        operation: &IssueStatusOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        let current = self.lookup_issue(token, &operation.issue)?;
        validate_remote_issue_scope(&self.config.scope, &current)?;
        let current_state = current
            .get("state")
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str);
        if current_state == Some(operation.state.as_str()) {
            return remote_receipt("issue-status", &operation.issue, false);
        }
        let response = self.graphql(
            token,
            "mutation WinWinCodeIssueUpdate($id: String!, $input: IssueUpdateInput!) { issueUpdate(id: $id, input: $input) { success issue { id state { id } } } }",
            &json!({"id": operation.issue, "input": {"stateId": operation.state}}),
        )?;
        let data = require_graphql_success(&response, self.clock.now_millis())?;
        let result = data.get("issueUpdate").ok_or_else(response_invalid)?;
        require_mutation_success(result)?;
        remote_receipt("issue-status", &operation.issue, true)
    }

    fn deliver_comment_create(
        &self,
        claim: &OutboundClaim,
        token: &LinearOAuthToken,
        operation: &CommentCreateOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        let marker = operation_marker(claim);
        if let Some(remote_id) = self.lookup_comment_marker(token, operation, &marker)? {
            return remote_receipt("comment", &remote_id, false);
        }
        let response = self.graphql(
            token,
            "mutation WinWinCodeCommentCreate($input: CommentCreateInput!) { commentCreate(input: $input) { success comment { id } } }",
            &json!({"input": {"issueId": operation.issue, "body": format!("{}\n\n{marker}", operation.body)}}),
        )?;
        let data = require_graphql_success(&response, self.clock.now_millis())?;
        let result = data.get("commentCreate").ok_or_else(response_invalid)?;
        require_mutation_success(result)?;
        let id = result
            .get("comment")
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .filter(|id| valid_linear_uuid(id))
            .ok_or_else(response_invalid)?;
        remote_receipt("comment", id, true)
    }

    fn lookup_issue_marker(
        &self,
        token: &LinearOAuthToken,
        marker: &str,
    ) -> Result<Option<String>, ConnectorCallError> {
        let mut cursor: Option<String> = None;
        for _ in 0..self.config.max_lookup_pages {
            let response = self.graphql(
                token,
                "query WinWinCodeIssueLookup($teamId: String!, $after: String) { team(id: $teamId) { issues(first: 50, after: $after) { nodes { id description project { id } } pageInfo { hasNextPage endCursor } } } }",
                &json!({"teamId": self.config.scope.team.as_str(), "after": cursor}),
            )?;
            let data = require_graphql_success(&response, self.clock.now_millis())?;
            let connection = data
                .get("team")
                .and_then(|value| value.get("issues"))
                .ok_or_else(response_invalid)?;
            let nodes = connection
                .get("nodes")
                .and_then(Value::as_array)
                .ok_or_else(response_invalid)?;
            if let Some(id) = find_marked_issue(nodes, &self.config.scope, marker) {
                return Ok(Some(id));
            }
            let Some(next) = next_cursor(connection)? else {
                return Ok(None);
            };
            cursor = Some(next);
        }
        Err(connector_error(
            ConnectorCallErrorKind::Retryable,
            "LINEAR_LOOKUP_BOUND_EXCEEDED",
        ))
    }

    fn lookup_issue(
        &self,
        token: &LinearOAuthToken,
        issue_id: &str,
    ) -> Result<Value, ConnectorCallError> {
        let response = self.graphql(
            token,
            "query WinWinCodeIssue($id: String!) { issue(id: $id) { id team { id } project { id } state { id } } }",
            &json!({"id": issue_id}),
        )?;
        let data = require_graphql_success(&response, self.clock.now_millis())?;
        data.get("issue").cloned().ok_or_else(response_invalid)
    }

    fn lookup_comment_marker(
        &self,
        token: &LinearOAuthToken,
        operation: &CommentCreateOperation,
        marker: &str,
    ) -> Result<Option<String>, ConnectorCallError> {
        let mut cursor: Option<String> = None;
        for _ in 0..self.config.max_lookup_pages {
            let response = self.graphql(
                token,
                "query WinWinCodeCommentLookup($id: String!, $after: String) { issue(id: $id) { id team { id } project { id } comments(first: 50, after: $after) { nodes { id body } pageInfo { hasNextPage endCursor } } } }",
                &json!({"id": operation.issue, "after": cursor}),
            )?;
            let data = require_graphql_success(&response, self.clock.now_millis())?;
            let issue = data.get("issue").ok_or_else(response_invalid)?;
            validate_remote_issue_scope(&self.config.scope, issue)?;
            let connection = issue.get("comments").ok_or_else(response_invalid)?;
            let nodes = connection
                .get("nodes")
                .and_then(Value::as_array)
                .ok_or_else(response_invalid)?;
            if let Some(id) = find_marked_comment(nodes, marker) {
                return Ok(Some(id));
            }
            let Some(next) = next_cursor(connection)? else {
                return Ok(None);
            };
            cursor = Some(next);
        }
        Err(connector_error(
            ConnectorCallErrorKind::Retryable,
            "LINEAR_LOOKUP_BOUND_EXCEEDED",
        ))
    }

    fn graphql(
        &self,
        token: &LinearOAuthToken,
        query: &str,
        variables: &Value,
    ) -> Result<LinearResponse, ConnectorCallError> {
        let authorization = format!("Bearer {}", token.value());
        let response = self
            .agent
            .post(&self.config.graphql_endpoint)
            .header("Accept", "application/json")
            .header("Authorization", &authorization)
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT)
            .send_json(json!({"query": query, "variables": variables}))
            .map_err(|_| {
                connector_error(
                    ConnectorCallErrorKind::Retryable,
                    "LINEAR_TRANSPORT_UNAVAILABLE",
                )
            })?;
        let status = response.status().as_u16();
        let reset_at_millis = response
            .headers()
            .get("x-ratelimit-endpoint-requests-reset")
            .or_else(|| response.headers().get("x-ratelimit-requests-reset"))
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let bytes = response
            .into_body()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(|_| {
                connector_error(
                    ConnectorCallErrorKind::Retryable,
                    "LINEAR_RESPONSE_UNREADABLE",
                )
            })?;
        let body = if bytes.is_empty() {
            None
        } else {
            Some(serde_json::from_slice(&bytes).map_err(|_| response_invalid())?)
        };
        Ok(LinearResponse {
            status,
            reset_at_millis,
            body,
        })
    }
}

fn find_marked_issue(
    nodes: &[Value],
    scope: &LinearConnectorScope,
    marker: &str,
) -> Option<String> {
    nodes.iter().find_map(|issue| {
        let marker_matches = issue
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|description| description.contains(marker));
        (marker_matches && remote_project_matches(scope, issue))
            .then(|| {
                issue
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| valid_linear_uuid(id))
                    .map(str::to_owned)
            })
            .flatten()
    })
}

fn find_marked_comment(nodes: &[Value], marker: &str) -> Option<String> {
    nodes.iter().find_map(|comment| {
        comment
            .get("body")
            .and_then(Value::as_str)
            .is_some_and(|body| body.contains(marker))
            .then(|| {
                comment
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| valid_linear_uuid(id))
                    .map(str::to_owned)
            })
            .flatten()
    })
}

struct ParsedWebhook<'a> {
    kind: LinearEventKind,
    action: LinearEventAction,
    resource_id: &'a str,
    issue_id: Option<&'a str>,
    ordering_key: String,
    timestamp_millis: u64,
}

fn parse_webhook_value<'a>(
    config: &LinearConnectorConfig,
    value: &'a Value,
) -> Result<ParsedWebhook<'a>, IntegrationError> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .and_then(LinearEventKind::parse)
        .ok_or_else(|| invalid("Linear webhook type is unsupported"))?;
    let action = value
        .get("action")
        .and_then(Value::as_str)
        .and_then(|value| LinearEventAction::parse(kind, value))
        .ok_or_else(|| invalid("Linear webhook action is unsupported"))?;
    let workspace_id = value
        .get("organizationId")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("Linear webhook workspace is missing"))?;
    if workspace_id != config.scope.workspace.as_str() {
        return Err(invalid("Linear webhook workspace does not match"));
    }
    validate_optional_webhook_scope(&config.scope, value)?;
    let timestamp_millis = value
        .get("webhookTimestamp")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0 && *value <= MAX_SAFE_INTEGER)
        .ok_or_else(|| invalid("Linear webhook timestamp is invalid"))?;
    let (resource_id, issue_id) = webhook_resource(kind, value)?;
    let ordering_key = format!("{}:{resource_id}", kind.canonical_name());
    Ok(ParsedWebhook {
        kind,
        action,
        resource_id,
        issue_id,
        ordering_key,
        timestamp_millis,
    })
}

fn webhook_resource(
    kind: LinearEventKind,
    value: &Value,
) -> Result<(&str, Option<&str>), IntegrationError> {
    if kind == LinearEventKind::OAuthApp {
        let id = value
            .get("oauthClientId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or_else(|| invalid("Linear OAuth app identity is invalid"))?;
        return Ok((id, None));
    }
    let data = value
        .get("data")
        .ok_or_else(|| invalid("Linear webhook data is missing"))?;
    let resource_id = data
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| valid_linear_uuid(id))
        .ok_or_else(|| invalid("Linear webhook resource identity is invalid"))?;
    let issue_id = if kind == LinearEventKind::Comment {
        Some(
            data.get("issueId")
                .and_then(Value::as_str)
                .filter(|id| valid_linear_uuid(id))
                .ok_or_else(|| invalid("Linear comment issue identity is invalid"))?,
        )
    } else {
        None
    };
    Ok((resource_id, issue_id))
}

fn validate_optional_webhook_scope(
    scope: &LinearConnectorScope,
    value: &Value,
) -> Result<(), IntegrationError> {
    let data = value.get("data");
    let team_id = data.and_then(|data| {
        data.get("teamId")
            .and_then(Value::as_str)
            .or_else(|| data.get("team")?.get("id")?.as_str())
    });
    if team_id.is_some_and(|value| value != scope.team_id().as_str()) {
        return Err(invalid("Linear webhook team does not match"));
    }
    let project_id = data.and_then(|data| {
        data.get("projectId")
            .and_then(Value::as_str)
            .or_else(|| data.get("project")?.get("id")?.as_str())
    });
    if project_id.is_some_and(|value| scope.project_id().is_none_or(|id| value != id.as_str())) {
        return Err(invalid("Linear webhook project does not match"));
    }
    Ok(())
}

fn require_graphql_success(
    response: &LinearResponse,
    now_millis: u64,
) -> Result<&Value, ConnectorCallError> {
    if response.status == 401 {
        return Err(connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "LINEAR_CREDENTIAL_REVOKED",
        ));
    }
    let errors = response
        .body
        .as_ref()
        .and_then(|body| body.get("errors"))
        .and_then(Value::as_array);
    if errors.is_some_and(|values| values.iter().any(is_rate_limited)) {
        return Err(rate_limit_error(response.reset_at_millis, now_millis));
    }
    if errors.is_some_and(|values| values.iter().any(is_credential_revoked)) {
        return Err(connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "LINEAR_CREDENTIAL_REVOKED",
        ));
    }
    if errors.is_some_and(|values| !values.is_empty()) {
        return Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "LINEAR_GRAPHQL_REJECTED",
        ));
    }
    match response.status {
        200 => response
            .body
            .as_ref()
            .and_then(|body| body.get("data"))
            .ok_or_else(response_invalid),
        429 => Err(rate_limit_error(response.reset_at_millis, now_millis)),
        408 | 425 | 500..=599 => Err(connector_error(
            ConnectorCallErrorKind::Retryable,
            "LINEAR_SERVICE_UNAVAILABLE",
        )),
        403 => Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "LINEAR_PERMISSION_DENIED",
        )),
        _ => Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "LINEAR_REQUEST_REJECTED",
        )),
    }
}

fn is_rate_limited(error: &Value) -> bool {
    graphql_error_code(error) == Some("RATELIMITED")
}

fn is_credential_revoked(error: &Value) -> bool {
    matches!(
        graphql_error_code(error),
        Some("AUTHENTICATION_ERROR" | "UNAUTHENTICATED")
    )
}

fn graphql_error_code(error: &Value) -> Option<&str> {
    error
        .get("extensions")
        .and_then(|value| value.get("code"))
        .and_then(Value::as_str)
}

fn rate_limit_error(reset_at_millis: Option<u64>, now_millis: u64) -> ConnectorCallError {
    let hinted = reset_at_millis
        .filter(|reset| *reset > now_millis && *reset <= MAX_SAFE_INTEGER)
        .map(|reset| reset - now_millis)
        .and_then(|delay| ConnectorCallError::retryable_after("LINEAR_RATE_LIMITED", delay).ok());
    hinted.unwrap_or_else(|| {
        connector_error(ConnectorCallErrorKind::Retryable, "LINEAR_RATE_LIMITED")
    })
}

fn next_cursor(connection: &Value) -> Result<Option<String>, ConnectorCallError> {
    let page = connection.get("pageInfo").ok_or_else(response_invalid)?;
    let has_next = page
        .get("hasNextPage")
        .and_then(Value::as_bool)
        .ok_or_else(response_invalid)?;
    if !has_next {
        return Ok(None);
    }
    let cursor = page
        .get("endCursor")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .ok_or_else(response_invalid)?;
    Ok(Some(cursor.to_owned()))
}

fn require_mutation_success(result: &Value) -> Result<(), ConnectorCallError> {
    if result.get("success").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "LINEAR_MUTATION_REJECTED",
        ))
    }
}

fn validate_remote_issue_scope(
    scope: &LinearConnectorScope,
    issue: &Value,
) -> Result<(), ConnectorCallError> {
    let team_matches = issue
        .get("team")
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        == Some(scope.team_id().as_str());
    if team_matches && remote_project_matches(scope, issue) {
        Ok(())
    } else {
        Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "LINEAR_SCOPE_MISMATCH",
        ))
    }
}

fn remote_project_matches(scope: &LinearConnectorScope, issue: &Value) -> bool {
    let actual = issue
        .get("project")
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str);
    match scope.project_id() {
        Some(expected) => actual == Some(expected.as_str()),
        None => actual.is_none(),
    }
}

fn option_matches(actual: Option<&str>, expected: Option<&LinearProjectId>) -> bool {
    match (actual, expected) {
        (Some(actual), Some(expected)) => actual == expected.as_str(),
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

fn validate_token(
    config: &LinearConnectorConfig,
    token: &LinearOAuthToken,
    now_millis: u64,
) -> Result<(), ConnectorCallError> {
    if now_millis == 0 || now_millis > MAX_SAFE_INTEGER {
        return Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "LINEAR_CLOCK_INVALID",
        ));
    }
    if token.workspace_id != config.scope.workspace {
        return Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "LINEAR_TOKEN_SCOPE_MISMATCH",
        ));
    }
    if token.expires_at_millis <= now_millis {
        return Err(connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "LINEAR_CREDENTIAL_REVOKED",
        ));
    }
    Ok(())
}

fn require_oauth_scope(
    token: &LinearOAuthToken,
    required: LinearOAuthScope,
) -> Result<(), ConnectorCallError> {
    if token.permits(LinearOAuthScope::Read) && token.permits(required) {
        Ok(())
    } else {
        Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "LINEAR_PERMISSION_DENIED",
        ))
    }
}

fn require_authority(
    config: &LinearConnectorConfig,
    authority: &ConnectorAuthority,
) -> Result<(), ConnectorCallError> {
    if matches_authority(config, authority) && authority.state() == crate::ConnectorState::Active {
        Ok(())
    } else if authority.state() == crate::ConnectorState::CredentialRevoked {
        Err(connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "LINEAR_CREDENTIAL_REVOKED",
        ))
    } else {
        Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "LINEAR_AUTHORITY_MISMATCH",
        ))
    }
}

fn matches_authority(config: &LinearConnectorConfig, authority: &ConnectorAuthority) -> bool {
    authority.integration_id() == &config.integration_id
        && authority.credential_reference_id() == &config.credential_reference_id
        && authority.protocol().as_str() == LINEAR_CONNECTOR_PROTOCOL
}

fn signature_credential_error(error: LinearCredentialError) -> SignatureVerificationError {
    match error.kind() {
        LinearCredentialErrorKind::Revoked => SignatureVerificationError::credential_revoked(),
        LinearCredentialErrorKind::PermissionDenied | LinearCredentialErrorKind::Unavailable => {
            SignatureVerificationError::rejected()
        }
    }
}

fn connector_credential_error(error: LinearCredentialError) -> ConnectorCallError {
    match error.kind() {
        LinearCredentialErrorKind::Revoked => connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "LINEAR_CREDENTIAL_REVOKED",
        ),
        LinearCredentialErrorKind::PermissionDenied => connector_error(
            ConnectorCallErrorKind::Permanent,
            "LINEAR_PERMISSION_DENIED",
        ),
        LinearCredentialErrorKind::Unavailable => connector_error(
            ConnectorCallErrorKind::Retryable,
            "LINEAR_CREDENTIAL_UNAVAILABLE",
        ),
    }
}

fn remote_receipt(
    resource_kind: &str,
    remote_id: &str,
    remote_write_performed: bool,
) -> Result<OutboundCallReceipt, ConnectorCallError> {
    let mut hash = Sha256::new();
    hash.update(b"winwincode.linear.remote-receipt.v1");
    hash.update([0]);
    hash.update(resource_kind.as_bytes());
    hash.update([0]);
    hash.update(remote_id.as_bytes());
    OutboundCallReceipt::try_new(
        Sha256Digest(format!("sha256:{:x}", hash.finalize())),
        remote_write_performed,
    )
    .map_err(|_| connector_error(ConnectorCallErrorKind::Permanent, "LINEAR_RESPONSE_INVALID"))
}

fn operation_marker(claim: &OutboundClaim) -> String {
    format!(
        "<!-- winwincode-integration:{} -->",
        claim.operation_key().digest().0
    )
}

fn response_invalid() -> ConnectorCallError {
    connector_error(ConnectorCallErrorKind::Retryable, "LINEAR_RESPONSE_INVALID")
}

fn connector_error(kind: ConnectorCallErrorKind, code: &str) -> ConnectorCallError {
    ConnectorCallError::try_new(kind, code).expect("static Linear error code")
}

fn invalid(message: &'static str) -> IntegrationError {
    IntegrationError::new(IntegrationErrorKind::Invalid, message)
}

fn validate_uuid(value: &str) -> Result<(), IntegrationError> {
    if valid_linear_uuid(value) {
        Ok(())
    } else {
        Err(invalid("Linear identity is invalid"))
    }
}

fn valid_linear_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }
        })
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
        Err(invalid("Linear credential reference is invalid"))
    }
}

fn validate_tls_roots(roots: &LinearTlsRoots) -> Result<(), IntegrationError> {
    let valid = match roots {
        LinearTlsRoots::WebPki => true,
        LinearTlsRoots::Specific(values) => {
            !values.is_empty()
                && values.len() <= 32
                && values
                    .iter()
                    .all(|value| !value.is_empty() && value.len() <= 65_536)
        }
    };
    if valid {
        Ok(())
    } else {
        Err(invalid("Linear TLS roots are invalid"))
    }
}

fn canonical_graphql_endpoint(value: &str) -> Result<String, IntegrationError> {
    let uri = ureq::http::Uri::from_str(value)
        .map_err(|_| invalid("Linear GraphQL endpoint is invalid"))?;
    let valid = uri.scheme_str() == Some("https")
        && uri.authority().is_some()
        && uri
            .authority()
            .is_none_or(|authority| !authority.as_str().contains('@'))
        && uri.query().is_none()
        && !value.contains('#')
        && uri.path().ends_with("/graphql");
    if valid {
        Ok(value.to_owned())
    } else {
        Err(invalid("Linear GraphQL endpoint is invalid"))
    }
}

fn decode_signature(value: &[u8]) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut decoded = [0_u8; 32];
    for (index, pair) in value.chunks_exact(2).enumerate() {
        decoded[index] = decode_hex(pair[0])?.checked_mul(16)? + decode_hex(pair[1])?;
    }
    Some(decoded)
}

const fn decode_hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.contains('\0')
}

fn valid_body(value: &str, max: usize) -> bool {
    value.len() <= max && !value.contains('\0')
}
