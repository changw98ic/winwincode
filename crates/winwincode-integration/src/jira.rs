// SPDX-License-Identifier: Apache-2.0

//! Jira Cloud protocol adapter over the durable Integration Framework.

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

/// Canonical Integration Framework protocol identifier for Jira Cloud.
pub const JIRA_CONNECTOR_PROTOCOL: &str = "jira.cloud.v1";
const USER_AGENT: &str = "WinWinCode-Jira-Enterprise-Connector";
const MAX_RESPONSE_BYTES: u64 = 2 * 1_024 * 1_024;
const MAX_LOOKUP_RESULTS: usize = 100;
const OPERATION_PROPERTY: &str = "winwincode.operation";

/// Canonical Jira Cloud site identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JiraSiteId(String);

impl JiraSiteId {
    /// Builds a bounded portable Jira Cloud site identity.
    ///
    /// # Errors
    ///
    /// Rejects empty, overlong, or non-portable identities.
    pub fn try_new(value: impl Into<String>) -> Result<Self, IntegrationError> {
        let value = value.into();
        if !valid_identifier(&value, 128) {
            return Err(invalid("Jira site identity is invalid"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical uppercase Jira project key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JiraProjectKey(String);

impl JiraProjectKey {
    /// Builds a Jira project key.
    ///
    /// # Errors
    ///
    /// Rejects keys outside Jira's portable uppercase form.
    pub fn try_new(value: impl Into<String>) -> Result<Self, IntegrationError> {
        let value = value.into();
        let valid = matches!(value.len(), 2..=20)
            && value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_uppercase())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit());
        if !valid {
            return Err(invalid("Jira project key is invalid"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Explicit TLS root configuration for Jira Cloud or a loopback fixture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JiraTlsRoots {
    WebPki,
    Specific(Vec<Vec<u8>>),
}

/// Credential-free Jira site/project connector configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JiraConnectorConfig {
    integration_id: EnterpriseIntegrationId,
    credential_reference_id: CredentialReferenceId,
    site_id: JiraSiteId,
    project_key: JiraProjectKey,
    api_base_url: String,
    tls_roots: JiraTlsRoots,
    request_timeout: Duration,
    max_lookup_pages: u16,
}

impl JiraConnectorConfig {
    /// Builds one exact Jira site/project boundary.
    ///
    /// # Errors
    ///
    /// Rejects invalid authority, URL, TLS roots, or project facts.
    pub fn try_new(
        integration_id: EnterpriseIntegrationId,
        credential_reference_id: CredentialReferenceId,
        site_id: JiraSiteId,
        project_key: JiraProjectKey,
        api_base_url: impl Into<String>,
        tls_roots: JiraTlsRoots,
    ) -> Result<Self, IntegrationError> {
        validate_integration_id(&integration_id)?;
        validate_prefixed_id(&credential_reference_id.0, "crd")?;
        validate_tls_roots(&tls_roots)?;
        let api_base_url = canonical_api_base_url(&api_base_url.into(), &site_id, &tls_roots)?;
        Ok(Self {
            integration_id,
            credential_reference_id,
            site_id,
            project_key,
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
    pub const fn site_id(&self) -> &JiraSiteId {
        &self.site_id
    }

    #[must_use]
    pub const fn project_key(&self) -> &JiraProjectKey {
        &self.project_key
    }
}

/// Jira OAuth/webhook credential resolution category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JiraCredentialErrorKind {
    Revoked,
    PermissionDenied,
    Unavailable,
}

/// Secret-safe Jira credential resolution error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JiraCredentialError {
    kind: JiraCredentialErrorKind,
}

impl JiraCredentialError {
    #[must_use]
    pub const fn new(kind: JiraCredentialErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(self) -> JiraCredentialErrorKind {
        self.kind
    }
}

impl fmt::Display for JiraCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Jira credential could not be resolved")
    }
}

impl std::error::Error for JiraCredentialError {}

/// Short-lived Jira webhook secret; never serializable or clonable.
pub struct JiraWebhookSecret(Vec<u8>);

impl JiraWebhookSecret {
    /// Builds opaque webhook secret material.
    ///
    /// # Errors
    ///
    /// Rejects empty or oversized values.
    pub fn try_new(value: impl AsRef<[u8]>) -> Result<Self, IntegrationError> {
        let value = value.as_ref();
        if value.is_empty() || value.len() > 4_096 {
            return Err(invalid("Jira webhook secret is invalid"));
        }
        Ok(Self(value.to_vec()))
    }

    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for JiraWebhookSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JiraWebhookSecret([REDACTED])")
    }
}

impl Drop for JiraWebhookSecret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Closed Jira OAuth permission required by the connector.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum JiraOAuthScope {
    ReadIssue,
    WriteIssue,
    ReadComment,
    WriteComment,
}

/// Short-lived OAuth token sealed to one Jira site and bounded project set.
pub struct JiraOAuthAccessToken {
    token: Vec<u8>,
    site_id: JiraSiteId,
    project_keys: BTreeSet<String>,
    scopes: BTreeSet<JiraOAuthScope>,
    expires_at_millis: u64,
}

impl JiraOAuthAccessToken {
    /// Builds an opaque site/project-scoped access token.
    ///
    /// # Errors
    ///
    /// Rejects invalid token bytes, empty project/scopes, or invalid expiry.
    pub fn try_new(
        token: impl AsRef<[u8]>,
        site_id: JiraSiteId,
        project_keys: impl IntoIterator<Item = JiraProjectKey>,
        scopes: impl IntoIterator<Item = JiraOAuthScope>,
        expires_at_millis: u64,
    ) -> Result<Self, IntegrationError> {
        let token = token.as_ref();
        let project_keys = project_keys
            .into_iter()
            .map(|key| key.0)
            .collect::<BTreeSet<_>>();
        let scopes = scopes.into_iter().collect::<BTreeSet<_>>();
        if token.is_empty()
            || token.len() > 4_096
            || !token.iter().all(|byte| matches!(byte, 0x21..=0x7e))
            || project_keys.is_empty()
            || scopes.is_empty()
            || expires_at_millis == 0
            || expires_at_millis > MAX_SAFE_INTEGER
        {
            return Err(invalid("Jira OAuth token is invalid"));
        }
        Ok(Self {
            token: token.to_vec(),
            site_id,
            project_keys,
            scopes,
            expires_at_millis,
        })
    }

    fn value(&self) -> &str {
        std::str::from_utf8(&self.token).expect("validated visible ASCII token")
    }
}

impl fmt::Debug for JiraOAuthAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JiraOAuthAccessToken")
            .field("site_id", &self.site_id)
            .field("project_keys", &self.project_keys)
            .field("scopes", &self.scopes)
            .field("expires_at_millis", &self.expires_at_millis)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl Drop for JiraOAuthAccessToken {
    fn drop(&mut self) {
        self.token.fill(0);
    }
}

/// Credential-owner port for Jira webhook and OAuth material.
pub trait JiraCredentialPort {
    /// Resolves the webhook secret only for raw-body authentication.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe credential error.
    fn resolve_webhook_secret(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<JiraWebhookSecret, JiraCredentialError>;

    /// Resolves one short-lived OAuth token for the configured site.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe credential error.
    fn resolve_oauth_token(
        &mut self,
        reference: &CredentialReferenceId,
        site_id: &JiraSiteId,
    ) -> Result<JiraOAuthAccessToken, JiraCredentialError>;
}

/// Time authority used only for OAuth expiry checks.
pub trait JiraClock {
    fn now_millis(&self) -> u64;
}

/// Exact Jira webhook headers before raw-body authentication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JiraWebhookHeaders {
    event_id: String,
    signature: Vec<u8>,
}

impl JiraWebhookHeaders {
    /// Builds a bounded Jira delivery identity and HMAC signature.
    ///
    /// # Errors
    ///
    /// Rejects missing, oversized, or malformed values.
    pub fn try_new(
        event_id: impl Into<String>,
        signature: impl AsRef<[u8]>,
    ) -> Result<Self, IntegrationError> {
        let event_id = event_id.into();
        let signature = signature.as_ref();
        if !valid_identifier(&event_id, 128)
            || signature.len() != 71
            || !signature.starts_with(b"sha256=")
        {
            return Err(invalid("Jira webhook headers are invalid"));
        }
        Ok(Self {
            event_id,
            signature: signature.to_vec(),
        })
    }
}

/// Builds Integration Framework requests from Jira webhook headers/body.
#[derive(Clone, Debug)]
pub struct JiraWebhookRequestFactory {
    config: JiraConnectorConfig,
}

impl JiraWebhookRequestFactory {
    #[must_use]
    pub const fn new(config: JiraConnectorConfig) -> Self {
        Self { config }
    }

    /// Builds one authenticated-request candidate with resource-local ordering.
    ///
    /// # Errors
    ///
    /// Rejects unknown event types or mismatched site/project facts.
    pub fn build(
        &self,
        scope: AuditScope,
        headers: JiraWebhookHeaders,
        payload: Vec<u8>,
        received_at_millis: u64,
    ) -> Result<InboundWebhookRequest, IntegrationError> {
        let value: Value = serde_json::from_slice(&payload)
            .map_err(|_| invalid("Jira webhook payload is invalid"))?;
        validate_webhook_scope(&self.config, &value)?;
        let event_type = webhook_event_type(&value)?;
        let ordering_key = webhook_ordering_key(event_type, &value)?;
        let provider_sequence = value
            .get("timestamp")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0 && *value <= MAX_SAFE_INTEGER)
            .ok_or_else(|| invalid("Jira webhook timestamp is invalid"))?;
        let metadata = InboundWebhookMetadata::try_new(
            event_type,
            headers.event_id,
            ordering_key,
            provider_sequence,
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

/// HMAC-SHA256 verifier using only the connector credential reference.
pub struct JiraWebhookVerifier<Credentials> {
    config: JiraConnectorConfig,
    credentials: Credentials,
}

impl<Credentials> JiraWebhookVerifier<Credentials> {
    #[must_use]
    pub const fn new(config: JiraConnectorConfig, credentials: Credentials) -> Self {
        Self {
            config,
            credentials,
        }
    }
}

impl<Credentials: JiraCredentialPort> WebhookSignatureVerifier
    for JiraWebhookVerifier<Credentials>
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

/// Closed Jira webhook resource kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JiraResourceKind {
    Issue,
    Comment,
}

/// Validated Jira webhook event supplied to a formal-command mapper.
pub struct JiraInboundEvent<'a> {
    event_type: &'a str,
    resource_kind: JiraResourceKind,
    actor_account_id: Option<&'a str>,
    site_id: &'a JiraSiteId,
    project_key: &'a JiraProjectKey,
    context: &'a InboundNormalizationContext,
    payload: &'a Value,
}

impl JiraInboundEvent<'_> {
    #[must_use]
    pub fn event_type(&self) -> &str {
        self.event_type
    }

    #[must_use]
    pub const fn resource_kind(&self) -> JiraResourceKind {
        self.resource_kind
    }

    #[must_use]
    pub const fn actor_account_id(&self) -> Option<&str> {
        self.actor_account_id
    }

    #[must_use]
    pub const fn site_id(&self) -> &JiraSiteId {
        self.site_id
    }

    #[must_use]
    pub const fn project_key(&self) -> &JiraProjectKey {
        self.project_key
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

/// Control Plane seam mapping one closed Jira event to a formal command.
pub trait JiraEventMapperPort {
    /// Maps a validated Jira event without copying Jira workflow state.
    ///
    /// # Errors
    ///
    /// Returns a stable secret-safe mapping failure.
    fn map_event(
        &mut self,
        authority: &ConnectorAuthority,
        event: &JiraInboundEvent<'_>,
    ) -> Result<NormalizedInboundEvent, ConnectorCallError>;
}

/// Jira protocol mapper and retry-stable outbound REST adapter.
pub struct JiraEnterpriseConnector<Credentials, Mapper, Clock> {
    config: JiraConnectorConfig,
    credentials: Credentials,
    mapper: Mapper,
    clock: Clock,
    agent: ureq::Agent,
}

impl<Credentials, Mapper, Clock> JiraEnterpriseConnector<Credentials, Mapper, Clock> {
    /// Builds a no-proxy, no-redirect, rustls-verified Jira connector.
    ///
    /// # Errors
    ///
    /// Rejects malformed explicit TLS roots.
    pub fn try_new(
        config: JiraConnectorConfig,
        credentials: Credentials,
        mapper: Mapper,
        clock: Clock,
    ) -> Result<Self, IntegrationError> {
        let roots = match &config.tls_roots {
            JiraTlsRoots::WebPki => ureq::tls::RootCerts::WebPki,
            JiraTlsRoots::Specific(values) => values
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
}

impl<Credentials: JiraCredentialPort, Mapper: JiraEventMapperPort, Clock: JiraClock> ConnectorPort
    for JiraEnterpriseConnector<Credentials, Mapper, Clock>
{
    fn normalize_inbound(
        &mut self,
        authority: &ConnectorAuthority,
        context: &InboundNormalizationContext,
        payload: &[u8],
    ) -> Result<NormalizedInboundEvent, ConnectorCallError> {
        require_authority(&self.config, authority)?;
        let value: Value = serde_json::from_slice(payload).map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "JIRA_PAYLOAD_INVALID")
        })?;
        validate_webhook_scope(&self.config, &value).map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "JIRA_SCOPE_MISMATCH")
        })?;
        let event_type = webhook_event_type(&value).map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "JIRA_EVENT_UNSUPPORTED")
        })?;
        let resource_kind = event_resource_kind(event_type).ok_or_else(|| {
            connector_error(ConnectorCallErrorKind::Permanent, "JIRA_EVENT_UNSUPPORTED")
        })?;
        let event = JiraInboundEvent {
            event_type,
            resource_kind,
            actor_account_id: value
                .get("user")
                .and_then(|user| user.get("accountId"))
                .and_then(Value::as_str),
            site_id: &self.config.site_id,
            project_key: &self.config.project_key,
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
            .resolve_oauth_token(
                claim.authority().credential_reference_id(),
                &self.config.site_id,
            )
            .map_err(connector_credential_error)?;
        validate_token(&self.config, &token, self.clock.now_millis())?;
        let operation = JiraOutboundOperation::parse(claim.operation_name(), claim.payload())?;
        require_scope(&token, operation.required_scopes())?;
        self.deliver_operation(claim, &token, &operation)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueCreateOperation {
    summary: String,
    description: Option<Value>,
    issue_type: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueUpdateOperation {
    issue_key: String,
    summary: Option<String>,
    description: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentCreateOperation {
    issue_key: String,
    body: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentUpdateOperation {
    issue_key: String,
    comment_id: String,
    body: Value,
}

enum JiraOutboundOperation {
    IssueCreate(IssueCreateOperation),
    IssueUpdate(IssueUpdateOperation),
    CommentCreate(CommentCreateOperation),
    CommentUpdate(CommentUpdateOperation),
}

impl JiraOutboundOperation {
    fn parse(name: &str, payload: &[u8]) -> Result<Self, ConnectorCallError> {
        let value = match name {
            "jira.issue.create.v1" => serde_json::from_slice(payload).map(Self::IssueCreate),
            "jira.issue.update.v1" => serde_json::from_slice(payload).map(Self::IssueUpdate),
            "jira.comment.create.v1" => serde_json::from_slice(payload).map(Self::CommentCreate),
            "jira.comment.update.v1" => serde_json::from_slice(payload).map(Self::CommentUpdate),
            _ => {
                return Err(connector_error(
                    ConnectorCallErrorKind::Permanent,
                    "JIRA_OPERATION_UNSUPPORTED",
                ));
            }
        }
        .map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "JIRA_OPERATION_INVALID")
        })?;
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ConnectorCallError> {
        let valid = match self {
            Self::IssueCreate(value) => {
                valid_text(&value.summary, 255)
                    && matches!(value.issue_type.as_str(), "Task" | "Bug" | "Story")
                    && value.description.as_ref().is_none_or(valid_document)
            }
            Self::IssueUpdate(value) => {
                valid_issue_key(&value.issue_key)
                    && value
                        .summary
                        .as_deref()
                        .is_none_or(|value| valid_text(value, 255))
                    && value.description.as_ref().is_none_or(valid_document)
                    && (value.summary.is_some() || value.description.is_some())
            }
            Self::CommentCreate(value) => {
                valid_issue_key(&value.issue_key) && valid_document(&value.body)
            }
            Self::CommentUpdate(value) => {
                valid_issue_key(&value.issue_key)
                    && valid_identifier(&value.comment_id, 64)
                    && valid_document(&value.body)
            }
        };
        if valid {
            Ok(())
        } else {
            Err(connector_error(
                ConnectorCallErrorKind::Permanent,
                "JIRA_OPERATION_INVALID",
            ))
        }
    }

    fn required_scopes(&self) -> &'static [JiraOAuthScope] {
        match self {
            Self::IssueCreate(_) | Self::IssueUpdate(_) => {
                &[JiraOAuthScope::ReadIssue, JiraOAuthScope::WriteIssue]
            }
            Self::CommentCreate(_) | Self::CommentUpdate(_) => {
                &[JiraOAuthScope::ReadComment, JiraOAuthScope::WriteComment]
            }
        }
    }
}

struct JiraResponse {
    status: u16,
    retry_after_seconds: Option<u64>,
    body: Option<Value>,
}

impl<Credentials, Mapper, Clock> JiraEnterpriseConnector<Credentials, Mapper, Clock> {
    fn deliver_operation(
        &self,
        claim: &OutboundClaim,
        token: &JiraOAuthAccessToken,
        operation: &JiraOutboundOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        match operation {
            JiraOutboundOperation::IssueCreate(value) => {
                self.deliver_issue_create(claim, token, value)
            }
            JiraOutboundOperation::IssueUpdate(value) => {
                self.deliver_issue_update(claim, token, value)
            }
            JiraOutboundOperation::CommentCreate(value) => {
                self.deliver_comment_create(claim, token, value)
            }
            JiraOutboundOperation::CommentUpdate(value) => {
                self.deliver_comment_update(claim, token, value)
            }
        }
    }

    fn deliver_issue_create(
        &self,
        claim: &OutboundClaim,
        token: &JiraOAuthAccessToken,
        operation: &IssueCreateOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        if let Some(remote_id) = self.lookup_issue_marker(token, claim)? {
            return remote_receipt("issue", &remote_id, false);
        }
        let mut fields = Map::new();
        fields.insert(
            "project".to_owned(),
            json!({"key": self.config.project_key.as_str()}),
        );
        fields.insert(
            "summary".to_owned(),
            Value::String(operation.summary.clone()),
        );
        fields.insert(
            "issuetype".to_owned(),
            json!({"name": operation.issue_type}),
        );
        fields.insert("labels".to_owned(), json!([operation_marker(claim)]));
        if let Some(description) = &operation.description {
            fields.insert("description".to_owned(), description.clone());
        }
        let response = self.request(
            token,
            "POST",
            "rest/api/3/issue",
            Some(&json!({
                "fields": fields,
                "properties": [{
                    "key": OPERATION_PROPERTY,
                    "value": {"key": claim.operation_key().digest().0}
                }]
            })),
        )?;
        require_success(&response, &[201])?;
        remote_receipt("issue", &response_identity(&response)?, true)
    }

    fn deliver_issue_update(
        &self,
        _claim: &OutboundClaim,
        token: &JiraOAuthAccessToken,
        operation: &IssueUpdateOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        require_project_issue(&self.config.project_key, &operation.issue_key)?;
        let mut fields = Map::new();
        if let Some(summary) = &operation.summary {
            fields.insert("summary".to_owned(), Value::String(summary.clone()));
        }
        if let Some(description) = &operation.description {
            fields.insert("description".to_owned(), description.clone());
        }
        let path = format!(
            "rest/api/3/issue/{}",
            encode_path_segment(&operation.issue_key)
        );
        let response = self.request(token, "PUT", &path, Some(&json!({"fields": fields})))?;
        require_success(&response, &[204])?;
        remote_receipt("issue", &operation.issue_key, true)
    }

    fn deliver_comment_create(
        &self,
        claim: &OutboundClaim,
        token: &JiraOAuthAccessToken,
        operation: &CommentCreateOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        require_project_issue(&self.config.project_key, &operation.issue_key)?;
        if let Some(remote_id) = self.lookup_comment_marker(token, claim, &operation.issue_key)? {
            return remote_receipt("comment", &remote_id, false);
        }
        let path = format!(
            "rest/api/3/issue/{}/comment",
            encode_path_segment(&operation.issue_key)
        );
        let response = self.request(
            token,
            "POST",
            &path,
            Some(&json!({
                "body": operation.body,
                "properties": [{
                    "key": OPERATION_PROPERTY,
                    "value": {"key": claim.operation_key().digest().0}
                }]
            })),
        )?;
        require_success(&response, &[201])?;
        remote_receipt("comment", &response_identity(&response)?, true)
    }

    fn deliver_comment_update(
        &self,
        _claim: &OutboundClaim,
        token: &JiraOAuthAccessToken,
        operation: &CommentUpdateOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        require_project_issue(&self.config.project_key, &operation.issue_key)?;
        let path = format!(
            "rest/api/3/issue/{}/comment/{}",
            encode_path_segment(&operation.issue_key),
            encode_path_segment(&operation.comment_id)
        );
        let response = self.request(token, "PUT", &path, Some(&json!({"body": operation.body})))?;
        require_success(&response, &[200])?;
        let remote_id = response
            .body
            .as_ref()
            .and_then(response_value_identity)
            .unwrap_or_else(|| operation.comment_id.clone());
        remote_receipt("comment", &remote_id, true)
    }

    fn lookup_issue_marker(
        &self,
        token: &JiraOAuthAccessToken,
        claim: &OutboundClaim,
    ) -> Result<Option<String>, ConnectorCallError> {
        let jql = format!(
            "project={} AND labels=\"{}\"",
            self.config.project_key.as_str(),
            operation_marker(claim)
        );
        let path = format!(
            "rest/api/3/search/jql?jql={}&maxResults=1",
            encode_query_component(&jql)
        );
        let response = self.request(token, "GET", &path, None)?;
        require_success(&response, &[200])?;
        let issues = response
            .body
            .as_ref()
            .and_then(|body| body.get("issues"))
            .and_then(Value::as_array)
            .ok_or_else(response_invalid)?;
        Ok(issues.first().and_then(response_value_identity))
    }

    fn lookup_comment_marker(
        &self,
        token: &JiraOAuthAccessToken,
        claim: &OutboundClaim,
        issue_key: &str,
    ) -> Result<Option<String>, ConnectorCallError> {
        for page in 0..self.config.max_lookup_pages {
            let start_at = usize::from(page) * MAX_LOOKUP_RESULTS;
            let path = format!(
                "rest/api/3/issue/{}/comment?expand=properties&maxResults={MAX_LOOKUP_RESULTS}&startAt={start_at}",
                encode_path_segment(issue_key),
            );
            let response = self.request(token, "GET", &path, None)?;
            require_success(&response, &[200])?;
            let body = response.body.as_ref().ok_or_else(response_invalid)?;
            let comments = body
                .get("comments")
                .and_then(Value::as_array)
                .ok_or_else(response_invalid)?;
            if let Some(remote_id) = comments.iter().find_map(|comment| {
                comment
                    .get("properties")
                    .and_then(Value::as_array)
                    .is_some_and(|properties| {
                        properties.iter().any(|property| {
                            property.get("key").and_then(Value::as_str) == Some(OPERATION_PROPERTY)
                                && property
                                    .get("value")
                                    .and_then(|value| value.get("key"))
                                    .and_then(Value::as_str)
                                    == Some(claim.operation_key().digest().0.as_str())
                        })
                    })
                    .then(|| response_value_identity(comment))
                    .flatten()
            }) {
                return Ok(Some(remote_id));
            }
            if comments.len() < MAX_LOOKUP_RESULTS {
                return Ok(None);
            }
        }
        Err(connector_error(
            ConnectorCallErrorKind::Retryable,
            "JIRA_LOOKUP_BOUND_EXCEEDED",
        ))
    }

    fn request(
        &self,
        token: &JiraOAuthAccessToken,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<JiraResponse, ConnectorCallError> {
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
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .header("User-Agent", USER_AGENT)
                .call(),
            ("POST", Some(body)) => self
                .agent
                .post(&url)
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .header("User-Agent", USER_AGENT)
                .send_json(body),
            ("PUT", Some(body)) => self
                .agent
                .put(&url)
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .header("User-Agent", USER_AGENT)
                .send_json(body),
            _ => {
                return Err(connector_error(
                    ConnectorCallErrorKind::Permanent,
                    "JIRA_REQUEST_INVALID",
                ));
            }
        }
        .map_err(|_| {
            connector_error(
                ConnectorCallErrorKind::Retryable,
                "JIRA_TRANSPORT_UNAVAILABLE",
            )
        })?;
        let status = response.status().as_u16();
        let retry_after_seconds = response
            .headers()
            .get("retry-after")
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
                    "JIRA_RESPONSE_UNREADABLE",
                )
            })?;
        let body = if bytes.is_empty() {
            None
        } else {
            serde_json::from_slice(&bytes).ok()
        };
        Ok(JiraResponse {
            status,
            retry_after_seconds,
            body,
        })
    }
}

fn require_success(response: &JiraResponse, accepted: &[u16]) -> Result<(), ConnectorCallError> {
    if accepted.contains(&response.status) {
        return Ok(());
    }
    if response.status == 429 {
        return Err(rate_limit_error(response.retry_after_seconds));
    }
    let (kind, code) = match response.status {
        401 => (
            ConnectorCallErrorKind::CredentialRevoked,
            "JIRA_CREDENTIAL_REVOKED",
        ),
        403 => (ConnectorCallErrorKind::Permanent, "JIRA_PERMISSION_DENIED"),
        408 | 409 | 425 | 500..=599 => (
            ConnectorCallErrorKind::Retryable,
            "JIRA_SERVICE_UNAVAILABLE",
        ),
        _ => (ConnectorCallErrorKind::Permanent, "JIRA_REQUEST_REJECTED"),
    };
    Err(connector_error(kind, code))
}

fn rate_limit_error(retry_after_seconds: Option<u64>) -> ConnectorCallError {
    retry_after_seconds
        .and_then(|seconds| seconds.checked_mul(1_000))
        .and_then(|millis| ConnectorCallError::retryable_after("JIRA_RATE_LIMITED", millis).ok())
        .unwrap_or_else(|| connector_error(ConnectorCallErrorKind::Retryable, "JIRA_RATE_LIMITED"))
}

fn validate_token(
    config: &JiraConnectorConfig,
    token: &JiraOAuthAccessToken,
    now_millis: u64,
) -> Result<(), ConnectorCallError> {
    if token.site_id != config.site_id || !token.project_keys.contains(config.project_key.as_str())
    {
        return Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "JIRA_SCOPE_MISMATCH",
        ));
    }
    if now_millis == 0 || now_millis >= token.expires_at_millis {
        return Err(connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "JIRA_CREDENTIAL_REVOKED",
        ));
    }
    Ok(())
}

fn require_scope(
    token: &JiraOAuthAccessToken,
    required: &[JiraOAuthScope],
) -> Result<(), ConnectorCallError> {
    if required.iter().all(|scope| token.scopes.contains(scope)) {
        Ok(())
    } else {
        Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "JIRA_PERMISSION_DENIED",
        ))
    }
}

fn require_authority(
    config: &JiraConnectorConfig,
    authority: &ConnectorAuthority,
) -> Result<(), ConnectorCallError> {
    if matches_authority(config, authority) && authority.state() == crate::ConnectorState::Active {
        Ok(())
    } else if authority.state() == crate::ConnectorState::CredentialRevoked {
        Err(connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "JIRA_CREDENTIAL_REVOKED",
        ))
    } else {
        Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "JIRA_AUTHORITY_MISMATCH",
        ))
    }
}

fn matches_authority(config: &JiraConnectorConfig, authority: &ConnectorAuthority) -> bool {
    authority.integration_id() == &config.integration_id
        && authority.credential_reference_id() == &config.credential_reference_id
        && authority.protocol().as_str() == JIRA_CONNECTOR_PROTOCOL
}

fn signature_credential_error(error: JiraCredentialError) -> SignatureVerificationError {
    match error.kind() {
        JiraCredentialErrorKind::Revoked => SignatureVerificationError::credential_revoked(),
        JiraCredentialErrorKind::PermissionDenied | JiraCredentialErrorKind::Unavailable => {
            SignatureVerificationError::rejected()
        }
    }
}

fn connector_credential_error(error: JiraCredentialError) -> ConnectorCallError {
    match error.kind() {
        JiraCredentialErrorKind::Revoked => connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "JIRA_CREDENTIAL_REVOKED",
        ),
        JiraCredentialErrorKind::PermissionDenied => {
            connector_error(ConnectorCallErrorKind::Permanent, "JIRA_PERMISSION_DENIED")
        }
        JiraCredentialErrorKind::Unavailable => connector_error(
            ConnectorCallErrorKind::Retryable,
            "JIRA_CREDENTIAL_UNAVAILABLE",
        ),
    }
}

fn validate_webhook_scope(
    config: &JiraConnectorConfig,
    value: &Value,
) -> Result<(), IntegrationError> {
    // Jira's documented webhook body does not contain a Cloud site identity.
    // The selected connector authority, callback route, and per-connector
    // credential reference bind that boundary before this adapter sees the
    // body. Some ingress deployments add a trusted `siteId` envelope field;
    // when present it must still agree with the configured Cloud site.
    let site_matches = match value.get("siteId") {
        None => true,
        Some(Value::String(site)) => site == config.site_id.as_str(),
        Some(_) => false,
    };
    let project = value
        .get("issue")
        .and_then(|issue| issue.get("fields"))
        .and_then(|fields| fields.get("project"))
        .and_then(|project| project.get("key"))
        .and_then(Value::as_str);
    if site_matches && project == Some(config.project_key.as_str()) {
        Ok(())
    } else {
        Err(invalid("Jira webhook scope does not match the connector"))
    }
}

fn webhook_event_type(value: &Value) -> Result<&str, IntegrationError> {
    let provider_event = value
        .get("webhookEvent")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("Jira webhook event is missing"))?;
    match provider_event {
        "jira:issue_created" => Ok("jira.issue.created"),
        "jira:issue_updated" => Ok("jira.issue.updated"),
        "jira:issue_deleted" => Ok("jira.issue.deleted"),
        "comment_created" => Ok("jira.comment.created"),
        "comment_updated" => Ok("jira.comment.updated"),
        "comment_deleted" => Ok("jira.comment.deleted"),
        _ => Err(invalid("Jira webhook event is unsupported")),
    }
}

fn event_resource_kind(event_type: &str) -> Option<JiraResourceKind> {
    match event_type {
        "jira.issue.created" | "jira.issue.updated" | "jira.issue.deleted" => {
            Some(JiraResourceKind::Issue)
        }
        "jira.comment.created" | "jira.comment.updated" | "jira.comment.deleted" => {
            Some(JiraResourceKind::Comment)
        }
        _ => None,
    }
}

fn webhook_ordering_key(event_type: &str, value: &Value) -> Result<String, IntegrationError> {
    let (resource, id) = match event_resource_kind(event_type) {
        Some(JiraResourceKind::Issue) => (
            "issue",
            value
                .get("issue")
                .and_then(|issue| issue.get("id"))
                .and_then(Value::as_str),
        ),
        Some(JiraResourceKind::Comment) => (
            "comment",
            value
                .get("comment")
                .and_then(|comment| comment.get("id"))
                .and_then(Value::as_str),
        ),
        None => return Err(invalid("Jira webhook event is unsupported")),
    };
    let id = id
        .filter(|value| valid_identifier(value, 128))
        .ok_or_else(|| invalid("Jira webhook resource identity is invalid"))?;
    Ok(format!("{resource}:{id}"))
}

fn require_project_issue(
    project_key: &JiraProjectKey,
    issue_key: &str,
) -> Result<(), ConnectorCallError> {
    let expected = format!("{}-", project_key.as_str());
    if issue_key.starts_with(&expected) && valid_issue_key(issue_key) {
        Ok(())
    } else {
        Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "JIRA_SCOPE_MISMATCH",
        ))
    }
}

fn operation_marker(claim: &OutboundClaim) -> String {
    format!(
        "wwc-{}",
        claim
            .operation_key()
            .digest()
            .0
            .strip_prefix("sha256:")
            .expect("validated SHA-256 digest")
    )
}

fn remote_receipt(
    resource_kind: &str,
    remote_id: &str,
    remote_write_performed: bool,
) -> Result<OutboundCallReceipt, ConnectorCallError> {
    let mut hash = Sha256::new();
    hash.update(b"winwincode.jira.remote-receipt.v1");
    hash.update([0]);
    hash.update(resource_kind.as_bytes());
    hash.update([0]);
    hash.update(remote_id.as_bytes());
    OutboundCallReceipt::try_new(
        Sha256Digest(format!("sha256:{:x}", hash.finalize())),
        remote_write_performed,
    )
    .map_err(|_| response_invalid())
}

fn response_identity(response: &JiraResponse) -> Result<String, ConnectorCallError> {
    response
        .body
        .as_ref()
        .and_then(response_value_identity)
        .ok_or_else(response_invalid)
}

fn response_value_identity(value: &Value) -> Option<String> {
    value
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| value.get("key").and_then(Value::as_str))
        .filter(|value| valid_identifier(value, 128))
        .map(str::to_owned)
}

fn decode_signature(value: &[u8]) -> Option<[u8; 32]> {
    let hex = value.strip_prefix(b"sha256=")?;
    if hex.len() != 64 {
        return None;
    }
    let mut output = [0_u8; 32];
    for (index, chunk) in hex.chunks_exact(2).enumerate() {
        output[index] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(output)
}

fn canonical_api_base_url(
    value: &str,
    site_id: &JiraSiteId,
    tls_roots: &JiraTlsRoots,
) -> Result<String, IntegrationError> {
    let uri =
        ureq::http::Uri::from_str(value).map_err(|_| invalid("Jira API base URL is invalid"))?;
    if uri.scheme_str() != Some("https")
        || uri.authority().is_none()
        || uri
            .authority()
            .is_some_and(|authority| authority.as_str().contains('@'))
        || uri.query().is_some()
        || value.contains('#')
    {
        return Err(invalid("Jira API base URL is invalid"));
    }
    let canonical = value.trim_end_matches('/');
    let expected_cloud = format!("https://api.atlassian.com/ex/jira/{}", site_id.as_str());
    let allowed = match tls_roots {
        JiraTlsRoots::WebPki => canonical == expected_cloud,
        JiraTlsRoots::Specific(_) => {
            uri.host() == Some("localhost") && uri.port_u16().is_some() && uri.path() == "/"
        }
    };
    if !allowed {
        return Err(invalid("Jira API base URL is invalid"));
    }
    Ok(format!("{canonical}/"))
}

fn validate_tls_roots(value: &JiraTlsRoots) -> Result<(), IntegrationError> {
    if let JiraTlsRoots::Specific(values) = value
        && (values.is_empty() || values.iter().any(Vec::is_empty))
    {
        return Err(invalid("Jira TLS roots are invalid"));
    }
    Ok(())
}

fn validate_prefixed_id(value: &str, prefix: &str) -> Result<(), IntegrationError> {
    let mut segments = value.split('_');
    let valid = segments.next() == Some(prefix)
        && segments.next().is_some_and(|tail| {
            tail.len() == 26 && tail.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        && segments.next().is_none();
    if valid {
        Ok(())
    } else {
        Err(invalid("Jira credential reference is invalid"))
    }
}

fn valid_identifier(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.'))
}

fn valid_issue_key(value: &str) -> bool {
    let Some((project, issue)) = value.rsplit_once('-') else {
        return false;
    };
    matches!(project.len(), 2..=20)
        && project
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        && issue.len() <= 20
        && !issue.is_empty()
        && issue.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_text(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn valid_document(value: &Value) -> bool {
    serde_json::to_vec(value).is_ok_and(|bytes| !bytes.is_empty() && bytes.len() <= 1_048_576)
}

fn encode_path_segment(value: &str) -> String {
    percent_encode(value, false)
}

fn encode_query_component(value: &str) -> String {
    percent_encode(value, true)
}

fn percent_encode(value: &str, query: bool) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        let safe = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || !query && byte == b':';
        if safe {
            output.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(output, "%{byte:02X}").expect("write percent encoding");
        }
    }
    output
}

fn response_invalid() -> ConnectorCallError {
    connector_error(ConnectorCallErrorKind::Retryable, "JIRA_RESPONSE_INVALID")
}

fn connector_error(kind: ConnectorCallErrorKind, code: &str) -> ConnectorCallError {
    ConnectorCallError::try_new(kind, code).expect("Jira error codes are portable")
}

fn invalid(message: &'static str) -> IntegrationError {
    IntegrationError::new(IntegrationErrorKind::Invalid, message)
}
