// SPDX-License-Identifier: Apache-2.0

//! Microsoft Teams protocol adapter over the durable Integration Framework.

use std::fmt::{self, Write as _};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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

/// Canonical Integration Framework protocol identifier for Microsoft Teams.
pub const MICROSOFT_TEAMS_CONNECTOR_PROTOCOL: &str = "microsoft.teams.graph.v1";

const MAX_OPAQUE_ID_BYTES: usize = 256;
const MAX_VALIDATION_TOKEN_BYTES: usize = 8_192;
const MAX_SECRET_BYTES: usize = 1_024;
const MAX_ACCESS_TOKEN_BYTES: usize = 16_384;
const MAX_MESSAGE_TEXT_BYTES: usize = 16_384;
const MAX_GRAPH_RESPONSE_BYTES: u64 = 2 * 1_024 * 1_024;
const GRAPH_LOOKUP_PAGE_SIZE: usize = 50;
const GRAPH_USER_AGENT: &str = "WinWinCode-Microsoft-Teams-Connector";

/// Canonical Microsoft Entra tenant GUID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TeamsTenantId(String);

impl TeamsTenantId {
    /// Builds a canonical lowercase GUID.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical GUID.
    pub fn try_new(value: impl Into<String>) -> Result<Self, IntegrationError> {
        let value = value.into();
        if !is_lowercase_guid(&value) {
            return Err(invalid("Microsoft Teams tenant identity is invalid"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical Microsoft Teams team identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TeamsTeamId(String);

impl TeamsTeamId {
    /// Builds a bounded Graph team identity.
    ///
    /// # Errors
    ///
    /// Rejects empty, unsafe, or overlong input.
    pub fn try_new(value: impl Into<String>) -> Result<Self, IntegrationError> {
        let value = value.into();
        validate_graph_id(&value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical Microsoft Teams channel identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TeamsChannelId(String);

impl TeamsChannelId {
    /// Builds a bounded Graph channel identity.
    ///
    /// # Errors
    ///
    /// Rejects empty, unsafe, or overlong input.
    pub fn try_new(value: impl Into<String>) -> Result<Self, IntegrationError> {
        let value = value.into();
        validate_graph_id(&value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Credential-free Teams connector authority configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamsConnectorConfig {
    integration: EnterpriseIntegrationId,
    credential_reference: CredentialReferenceId,
    tenant: TeamsTenantId,
    team: TeamsTeamId,
    channel: TeamsChannelId,
}

impl TeamsConnectorConfig {
    /// Binds one connector to exactly one tenant/team/channel scope.
    ///
    /// # Errors
    ///
    /// Rejects invalid Integration Framework or credential identities.
    pub fn try_new(
        integration_id: EnterpriseIntegrationId,
        credential_reference_id: CredentialReferenceId,
        tenant_id: TeamsTenantId,
        team_id: TeamsTeamId,
        channel_id: TeamsChannelId,
    ) -> Result<Self, IntegrationError> {
        validate_integration_id(&integration_id)?;
        validate_prefixed_id(&credential_reference_id.0, "crd")?;
        Ok(Self {
            integration: integration_id,
            credential_reference: credential_reference_id,
            tenant: tenant_id,
            team: team_id,
            channel: channel_id,
        })
    }

    #[must_use]
    pub const fn integration_id(&self) -> &EnterpriseIntegrationId {
        &self.integration
    }

    #[must_use]
    pub const fn credential_reference_id(&self) -> &CredentialReferenceId {
        &self.credential_reference
    }

    #[must_use]
    pub const fn tenant_id(&self) -> &TeamsTenantId {
        &self.tenant
    }

    #[must_use]
    pub const fn team_id(&self) -> &TeamsTeamId {
        &self.team
    }

    #[must_use]
    pub const fn channel_id(&self) -> &TeamsChannelId {
        &self.channel
    }
}

/// Microsoft Graph subscription validation challenge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamsGraphValidationChallenge(String);

impl TeamsGraphValidationChallenge {
    /// Parses one percent-encoded `validationToken` query value.
    ///
    /// # Errors
    ///
    /// Rejects malformed encoding, controls, empty values, or oversized tokens.
    pub fn try_from_query_value(value: &str) -> Result<Self, IntegrationError> {
        let decoded = percent_decode(value)?;
        validate_plain_token(&decoded)?;
        Ok(Self(decoded))
    }

    /// Produces the exact plain-text Graph validation response.
    #[must_use]
    pub fn response(&self) -> TeamsGraphValidationResponse {
        TeamsGraphValidationResponse(self.0.as_bytes().to_vec())
    }
}

/// Exact plain-text response for a Graph subscription validation request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamsGraphValidationResponse(Vec<u8>);

impl TeamsGraphValidationResponse {
    #[must_use]
    pub const fn content_type(&self) -> &'static str {
        "text/plain"
    }

    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.0
    }
}

/// Secret Graph webhook client-state value.
pub struct TeamsGraphClientState(Vec<u8>);

impl TeamsGraphClientState {
    /// Builds a bounded, non-empty secret.
    ///
    /// # Errors
    ///
    /// Rejects empty or oversized values.
    pub fn try_new(value: impl AsRef<[u8]>) -> Result<Self, IntegrationError> {
        let value = value.as_ref();
        if value.is_empty() || value.len() > MAX_SECRET_BYTES {
            return Err(invalid("Microsoft Graph client state is invalid"));
        }
        Ok(Self(value.to_vec()))
    }

    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for TeamsGraphClientState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TeamsGraphClientState([REDACTED])")
    }
}

impl Drop for TeamsGraphClientState {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Secret Microsoft Graph access token.
pub struct TeamsGraphAccessToken(Vec<u8>);

impl TeamsGraphAccessToken {
    /// Builds a bounded token.
    ///
    /// # Errors
    ///
    /// Rejects empty, control-containing, or oversized input.
    pub fn try_new(value: impl Into<String>) -> Result<Self, IntegrationError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_ACCESS_TOKEN_BYTES
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(invalid("Microsoft Graph access token is invalid"));
        }
        Ok(Self(value.into_bytes()))
    }

    #[must_use]
    pub fn expose_to_transport(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for TeamsGraphAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TeamsGraphAccessToken([REDACTED])")
    }
}

impl Drop for TeamsGraphAccessToken {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Credential lookup failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeamsCredentialErrorKind {
    Unavailable,
    Revoked,
}

/// Secret-safe credential lookup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TeamsCredentialError(TeamsCredentialErrorKind);

impl TeamsCredentialError {
    #[must_use]
    pub const fn unavailable() -> Self {
        Self(TeamsCredentialErrorKind::Unavailable)
    }

    #[must_use]
    pub const fn revoked() -> Self {
        Self(TeamsCredentialErrorKind::Revoked)
    }

    #[must_use]
    pub const fn kind(self) -> TeamsCredentialErrorKind {
        self.0
    }
}

impl fmt::Display for TeamsCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Microsoft Teams credential lookup failed")
    }
}

impl std::error::Error for TeamsCredentialError {}

/// Credential authority used by inbound validation and outbound Graph calls.
pub trait TeamsCredentialPort {
    /// Resolves the Graph subscription client-state secret.
    ///
    /// # Errors
    ///
    /// Returns only a stable missing/revoked result.
    fn resolve_webhook_client_state(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<TeamsGraphClientState, TeamsCredentialError>;

    /// Resolves a tenant-bound Graph access token.
    ///
    /// # Errors
    ///
    /// Returns only a stable missing/revoked result.
    fn resolve_access_token(
        &mut self,
        reference: &CredentialReferenceId,
        tenant_id: &TeamsTenantId,
    ) -> Result<TeamsGraphAccessToken, TeamsCredentialError>;
}

/// Verified facts from a Microsoft Graph notification JWT.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamsGraphTokenClaims {
    tenant_id: TeamsTenantId,
    application_id: String,
}

impl TeamsGraphTokenClaims {
    /// Builds identity facts only after a JWT implementation has verified its signature,
    /// issuer, audience, lifetime, and application identity.
    ///
    /// # Errors
    ///
    /// Rejects an unsafe application identity.
    pub fn try_new(
        tenant_id: TeamsTenantId,
        application_id: impl Into<String>,
    ) -> Result<Self, IntegrationError> {
        let application_id = application_id.into();
        validate_graph_id(&application_id)?;
        Ok(Self {
            tenant_id,
            application_id,
        })
    }

    #[must_use]
    pub const fn tenant_id(&self) -> &TeamsTenantId {
        &self.tenant_id
    }

    #[must_use]
    pub fn application_id(&self) -> &str {
        &self.application_id
    }
}

/// Secret-safe JWT validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeamsGraphTokenValidationError {
    Rejected,
    Unavailable,
}

/// Port implemented by the production Entra/JWKS validator.
pub trait TeamsGraphTokenValidatorPort {
    /// Validates a Graph notification token against issuer keys, audience, and time.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe rejected/unavailable result.
    fn validate_notification_token(
        &mut self,
        token: &str,
    ) -> Result<TeamsGraphTokenClaims, TeamsGraphTokenValidationError>;
}

/// Builds Integration Framework requests from one-item Graph notification batches.
#[derive(Clone, Debug)]
pub struct TeamsGraphWebhookRequestFactory {
    config: TeamsConnectorConfig,
}

impl TeamsGraphWebhookRequestFactory {
    #[must_use]
    pub const fn new(config: TeamsConnectorConfig) -> Self {
        Self { config }
    }

    /// Builds a raw request while preserving the client state and validation JWT only ephemerally.
    ///
    /// # Errors
    ///
    /// Rejects batches, unsupported notifications, and foreign tenant/team/channel scope.
    pub fn build(
        &self,
        scope: AuditScope,
        payload: Vec<u8>,
        received_at_millis: u64,
    ) -> Result<InboundWebhookRequest, IntegrationError> {
        let notification = parse_notification(&payload)?;
        require_notification_scope(&self.config, &notification)?;
        let token = notification
            .validation_token
            .as_deref()
            .ok_or_else(|| invalid("Microsoft Graph validation token is missing"))?;
        validate_plain_token(token)?;
        let proof = serde_json::to_vec(&WebhookProof {
            client_state: &notification.item.client_state,
            validation_token: token,
        })
        .map_err(|_| invalid("Microsoft Graph webhook proof is invalid"))?;
        let metadata = InboundWebhookMetadata::try_new(
            "teams.graph.change",
            notification_external_id(&notification.item),
            notification.item.subscription_id.clone(),
            notification.item.sequence_number,
            received_at_millis,
        )?;
        InboundWebhookRequest::try_new(
            self.config.integration.clone(),
            scope,
            metadata,
            proof,
            payload,
        )
    }
}

/// Graph client-state and JWT verifier.
pub struct TeamsGraphWebhookVerifier<Credentials, Tokens> {
    config: TeamsConnectorConfig,
    credentials: Credentials,
    tokens: Tokens,
}

impl<Credentials, Tokens> TeamsGraphWebhookVerifier<Credentials, Tokens> {
    #[must_use]
    pub const fn new(
        config: TeamsConnectorConfig,
        credentials: Credentials,
        tokens: Tokens,
    ) -> Self {
        Self {
            config,
            credentials,
            tokens,
        }
    }

    #[must_use]
    pub fn into_parts(self) -> (Credentials, Tokens) {
        (self.credentials, self.tokens)
    }
}

impl<Credentials: TeamsCredentialPort, Tokens: TeamsGraphTokenValidatorPort>
    WebhookSignatureVerifier for TeamsGraphWebhookVerifier<Credentials, Tokens>
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
        let proof: OwnedWebhookProof = serde_json::from_slice(signature)
            .map_err(|_| SignatureVerificationError::rejected())?;
        let notification =
            parse_notification(payload).map_err(|_| SignatureVerificationError::rejected())?;
        if notification.item.client_state != proof.client_state
            || notification.validation_token.as_deref() != Some(proof.validation_token.as_str())
        {
            return Err(SignatureVerificationError::rejected());
        }
        let expected = self
            .credentials
            .resolve_webhook_client_state(authority.credential_reference_id())
            .map_err(signature_credential_error)?;
        if !constant_time_equal(expected.bytes(), proof.client_state.as_bytes()) {
            return Err(SignatureVerificationError::rejected());
        }
        let claims = self
            .tokens
            .validate_notification_token(&proof.validation_token)
            .map_err(|_| SignatureVerificationError::rejected())?;
        if claims.tenant_id() != &self.config.tenant
            || notification.item.tenant_id != self.config.tenant
        {
            return Err(SignatureVerificationError::rejected());
        }
        Ok(())
    }
}

/// Provider response category for one Graph outbound call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeamsGraphCallErrorKind {
    RateLimited,
    Retryable,
    Permanent,
    CredentialRevoked,
}

/// Secret-safe Microsoft Graph call failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TeamsGraphCallError {
    kind: TeamsGraphCallErrorKind,
    retry_after_millis: Option<u64>,
}

impl TeamsGraphCallError {
    /// Builds a non-rate-limit Graph error.
    ///
    /// # Errors
    ///
    /// Rejects `RateLimited`, which requires a retry lower bound.
    pub fn try_new(kind: TeamsGraphCallErrorKind) -> Result<Self, IntegrationError> {
        if kind == TeamsGraphCallErrorKind::RateLimited {
            return Err(invalid("Microsoft Graph rate limit requires Retry-After"));
        }
        Ok(Self {
            kind,
            retry_after_millis: None,
        })
    }

    /// Builds a Graph 429 response with a bounded Retry-After lower bound.
    ///
    /// # Errors
    ///
    /// Rejects zero or an unsafe delay.
    pub fn rate_limited(retry_after_millis: u64) -> Result<Self, IntegrationError> {
        if retry_after_millis == 0 || retry_after_millis > MAX_SAFE_INTEGER {
            return Err(invalid("Microsoft Graph Retry-After is invalid"));
        }
        Ok(Self {
            kind: TeamsGraphCallErrorKind::RateLimited,
            retry_after_millis: Some(retry_after_millis),
        })
    }

    #[must_use]
    pub const fn kind(&self) -> TeamsGraphCallErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn retry_after_millis(&self) -> Option<u64> {
        self.retry_after_millis
    }
}

impl fmt::Display for TeamsGraphCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Microsoft Graph call failed")
    }
}

impl std::error::Error for TeamsGraphCallError {}

/// Exact provider message passed to a Graph transport implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamsGraphOutboundMessage {
    tenant_id: TeamsTenantId,
    team_id: TeamsTeamId,
    channel_id: TeamsChannelId,
    operation_key: Sha256Digest,
    body: Vec<u8>,
}

impl TeamsGraphOutboundMessage {
    #[must_use]
    pub const fn tenant_id(&self) -> &TeamsTenantId {
        &self.tenant_id
    }

    #[must_use]
    pub const fn team_id(&self) -> &TeamsTeamId {
        &self.team_id
    }

    #[must_use]
    pub const fn channel_id(&self) -> &TeamsChannelId {
        &self.channel_id
    }

    #[must_use]
    pub const fn operation_key(&self) -> &Sha256Digest {
        &self.operation_key
    }

    #[must_use]
    pub fn canonical_body(&self) -> &[u8] {
        &self.body
    }
}

/// Provider receipt returned by an idempotency-aware Graph transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamsGraphMessageReceipt {
    message_id: String,
    remote_write_performed: bool,
}

impl TeamsGraphMessageReceipt {
    /// Builds a bounded provider message receipt.
    ///
    /// # Errors
    ///
    /// Rejects an invalid Graph message identity.
    pub fn try_new(
        message_id: impl Into<String>,
        remote_write_performed: bool,
    ) -> Result<Self, IntegrationError> {
        let message_id = message_id.into();
        validate_graph_id(&message_id)?;
        Ok(Self {
            message_id,
            remote_write_performed,
        })
    }

    #[must_use]
    pub fn message_id(&self) -> &str {
        &self.message_id
    }

    #[must_use]
    pub const fn remote_write_performed(&self) -> bool {
        self.remote_write_performed
    }
}

/// Explicit TLS roots for Microsoft Graph or a loopback Graph sandbox.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TeamsGraphTlsRoots {
    WebPki,
    Specific(Vec<Vec<u8>>),
}

/// No-proxy, no-redirect Microsoft Graph transport with marker reconciliation.
pub struct TeamsGraphHttpTransport {
    api_base_url: String,
    agent: ureq::Agent,
    max_lookup_pages: u16,
}

impl TeamsGraphHttpTransport {
    /// Builds a rustls-verified Microsoft Graph transport.
    ///
    /// # Errors
    ///
    /// Rejects insecure/credential-bearing endpoints or malformed explicit TLS roots.
    pub fn try_new(
        api_base_url: impl Into<String>,
        tls_roots: TeamsGraphTlsRoots,
    ) -> Result<Self, IntegrationError> {
        validate_tls_roots(&tls_roots)?;
        let api_base_url = canonical_api_base_url(&api_base_url.into())?;
        let roots = match tls_roots {
            TeamsGraphTlsRoots::WebPki => ureq::tls::RootCerts::WebPki,
            TeamsGraphTlsRoots::Specific(values) => values
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
            .timeout_global(Some(Duration::from_secs(30)))
            .tls_config(tls)
            .build()
            .into();
        Ok(Self {
            api_base_url,
            agent,
            max_lookup_pages: 20,
        })
    }

    fn find_message(
        &self,
        token: &TeamsGraphAccessToken,
        message: &TeamsGraphOutboundMessage,
    ) -> Result<Option<String>, TeamsGraphCallError> {
        let marker = operation_marker(message.operation_key());
        let mut url = self.messages_url(message);
        let _ = write!(url, "?$top={GRAPH_LOOKUP_PAGE_SIZE}");
        for _ in 0..self.max_lookup_pages {
            let response = self.request(token, "GET", &url, None, message.operation_key())?;
            require_graph_success(&response, &[200])?;
            let body = response.body.as_ref().ok_or_else(graph_response_invalid)?;
            let values = body
                .get("value")
                .and_then(Value::as_array)
                .ok_or_else(graph_response_invalid)?;
            if let Some(remote_id) = values.iter().find_map(|value| {
                value
                    .get("body")
                    .and_then(|body| body.get("content"))
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.contains(&marker))
                    .then(|| value.get("id").and_then(Value::as_str))
                    .flatten()
                    .filter(|value| validate_graph_id(value).is_ok())
                    .map(str::to_owned)
            }) {
                return Ok(Some(remote_id));
            }
            let Some(next_link) = body.get("@odata.nextLink").and_then(Value::as_str) else {
                return Ok(None);
            };
            url = self.require_next_link(next_link)?;
        }
        Err(graph_error(TeamsGraphCallErrorKind::Retryable))
    }

    fn create_message(
        &self,
        token: &TeamsGraphAccessToken,
        message: &TeamsGraphOutboundMessage,
    ) -> Result<TeamsGraphMessageReceipt, TeamsGraphCallError> {
        let marker = operation_marker(message.operation_key());
        let client_request_id = client_request_id(message.operation_key());
        let card =
            std::str::from_utf8(message.canonical_body()).map_err(|_| graph_response_invalid())?;
        let body = json!({
            "attachments": [{
                "content": card,
                "contentType": "application/vnd.microsoft.card.adaptive",
                "id": client_request_id,
            }],
            "body": {
                "content": format!(
                    "{marker}<attachment id=\"{client_request_id}\"></attachment>"
                ),
                "contentType": "html",
            },
        });
        let response = self.request(
            token,
            "POST",
            &self.messages_url(message),
            Some(&body),
            message.operation_key(),
        )?;
        require_graph_success(&response, &[201])?;
        let remote_id = response
            .body
            .as_ref()
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(graph_response_invalid)?;
        TeamsGraphMessageReceipt::try_new(remote_id, true).map_err(|_| graph_response_invalid())
    }

    fn messages_url(&self, message: &TeamsGraphOutboundMessage) -> String {
        format!(
            "{}v1.0/teams/{}/channels/{}/messages",
            self.api_base_url,
            encode_path_segment(message.team_id().as_str()),
            encode_path_segment(message.channel_id().as_str())
        )
    }

    fn require_next_link(&self, value: &str) -> Result<String, TeamsGraphCallError> {
        if value.starts_with(&self.api_base_url)
            && canonical_absolute_url(value).is_ok()
            && !value.contains('#')
        {
            Ok(value.to_owned())
        } else {
            Err(graph_response_invalid())
        }
    }

    fn request(
        &self,
        token: &TeamsGraphAccessToken,
        method: &str,
        url: &str,
        body: Option<&Value>,
        operation_key: &Sha256Digest,
    ) -> Result<TeamsGraphHttpResponse, TeamsGraphCallError> {
        let token = std::str::from_utf8(token.expose_to_transport())
            .map_err(|_| graph_error(TeamsGraphCallErrorKind::CredentialRevoked))?;
        let authorization = format!("Bearer {token}");
        let client_request_id = client_request_id(operation_key);
        let response = match (method, body) {
            ("GET", None) => self
                .agent
                .get(url)
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .header("client-request-id", &client_request_id)
                .header("User-Agent", GRAPH_USER_AGENT)
                .call(),
            ("POST", Some(body)) => self
                .agent
                .post(url)
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .header("client-request-id", &client_request_id)
                .header("User-Agent", GRAPH_USER_AGENT)
                .send_json(body),
            _ => return Err(graph_error(TeamsGraphCallErrorKind::Permanent)),
        }
        .map_err(|_| graph_error(TeamsGraphCallErrorKind::Retryable))?;
        let status = response.status().as_u16();
        let retry_after_seconds = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let bytes = response
            .into_body()
            .with_config()
            .limit(MAX_GRAPH_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(|_| graph_error(TeamsGraphCallErrorKind::Retryable))?;
        let body = if bytes.is_empty() {
            None
        } else {
            serde_json::from_slice(&bytes).ok()
        };
        Ok(TeamsGraphHttpResponse {
            status,
            retry_after_seconds,
            body,
        })
    }
}

impl TeamsGraphTransportPort for TeamsGraphHttpTransport {
    fn deliver_message(
        &mut self,
        access_token: &TeamsGraphAccessToken,
        message: &TeamsGraphOutboundMessage,
    ) -> Result<TeamsGraphMessageReceipt, TeamsGraphCallError> {
        if let Some(remote_id) = self.find_message(access_token, message)? {
            return TeamsGraphMessageReceipt::try_new(remote_id, false)
                .map_err(|_| graph_response_invalid());
        }
        self.create_message(access_token, message)
    }
}

struct TeamsGraphHttpResponse {
    status: u16,
    retry_after_seconds: Option<u64>,
    body: Option<Value>,
}

/// Graph transport seam. Implementations must reconcile `operation_key` before creating a message.
pub trait TeamsGraphTransportPort {
    /// Delivers or finds the exact operation without performing a duplicate remote write.
    ///
    /// # Errors
    ///
    /// Returns a typed Graph failure; 429 must preserve Retry-After.
    fn deliver_message(
        &mut self,
        access_token: &TeamsGraphAccessToken,
        message: &TeamsGraphOutboundMessage,
    ) -> Result<TeamsGraphMessageReceipt, TeamsGraphCallError>;
}

/// Teams inbound normalizer and retry-stable outbound adapter.
pub struct TeamsEnterpriseConnector<Credentials, Transport> {
    config: TeamsConnectorConfig,
    credentials: Credentials,
    transport: Transport,
}

impl<Credentials, Transport> TeamsEnterpriseConnector<Credentials, Transport> {
    #[must_use]
    pub const fn new(
        config: TeamsConnectorConfig,
        credentials: Credentials,
        transport: Transport,
    ) -> Self {
        Self {
            config,
            credentials,
            transport,
        }
    }

    #[must_use]
    pub fn into_parts(self) -> (Credentials, Transport) {
        (self.credentials, self.transport)
    }
}

impl<Credentials: TeamsCredentialPort, Transport: TeamsGraphTransportPort> ConnectorPort
    for TeamsEnterpriseConnector<Credentials, Transport>
{
    fn normalize_inbound(
        &mut self,
        authority: &ConnectorAuthority,
        context: &InboundNormalizationContext,
        payload: &[u8],
    ) -> Result<NormalizedInboundEvent, ConnectorCallError> {
        require_authority(&self.config, authority)?;
        if context.event_type() != "teams.graph.change" {
            return Err(connector_error(
                ConnectorCallErrorKind::Permanent,
                "TEAMS_EVENT_UNSUPPORTED",
            ));
        }
        let notification = parse_notification(payload).map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "TEAMS_PAYLOAD_INVALID")
        })?;
        require_notification_scope(&self.config, &notification).map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "TEAMS_SCOPE_MISMATCH")
        })?;
        normalize_notification(context, &notification.item)
    }

    fn deliver_outbound(
        &mut self,
        claim: &OutboundClaim,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        require_authority(&self.config, claim.authority())?;
        let operation = TeamsOutboundOperation::parse(claim.operation_name(), claim.payload())?;
        operation.require_scope(&self.config)?;
        let access_token = self
            .credentials
            .resolve_access_token(
                claim.authority().credential_reference_id(),
                &self.config.tenant,
            )
            .map_err(connector_credential_error)?;
        let message = operation.message(claim)?;
        let remote = self
            .transport
            .deliver_message(&access_token, &message)
            .map_err(graph_call_error)?;
        remote_receipt(&remote)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct GraphNotificationEnvelope {
    value: Vec<GraphNotificationItem>,
    #[serde(default)]
    validation_tokens: Vec<String>,
    #[serde(default, rename = "@odata.context")]
    odata_context: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct GraphNotificationItem {
    subscription_id: String,
    #[serde(default, rename = "subscriptionExpirationDateTime")]
    subscription_expiration_date_time: Option<String>,
    change_type: String,
    client_state: String,
    tenant_id: TeamsTenantId,
    resource: String,
    sequence_number: u64,
    resource_data: GraphResourceData,
    #[serde(default, rename = "lifecycleEvent")]
    lifecycle_event: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct GraphResourceData {
    #[serde(default, rename = "@odata.id")]
    odata_id: Option<String>,
    #[serde(default, rename = "@odata.type")]
    odata_type: Option<String>,
    id: String,
    team_id: TeamsTeamId,
    channel_id: TeamsChannelId,
    from_user_id: String,
    action: String,
    interaction_id: String,
    expires_at_millis: u64,
}

struct ParsedNotification {
    item: GraphNotificationItem,
    validation_token: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebhookProof<'a> {
    client_state: &'a str,
    validation_token: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct OwnedWebhookProof {
    client_state: String,
    validation_token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct TeamsOutboundPayload {
    tenant_id: TeamsTenantId,
    team_id: TeamsTeamId,
    channel_id: TeamsChannelId,
    interaction_id: String,
    title: String,
    body: String,
    expires_at_millis: u64,
}

enum TeamsOutboundOperation {
    Attention(TeamsOutboundPayload),
    Approval(TeamsOutboundPayload),
}

impl TeamsOutboundOperation {
    fn parse(name: &str, payload: &[u8]) -> Result<Self, ConnectorCallError> {
        let parsed: TeamsOutboundPayload = serde_json::from_slice(payload).map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "TEAMS_PAYLOAD_INVALID")
        })?;
        validate_outbound_payload(&parsed)?;
        match name {
            "teams.attention.notify" => Ok(Self::Attention(parsed)),
            "teams.approval.notify" => Ok(Self::Approval(parsed)),
            _ => Err(connector_error(
                ConnectorCallErrorKind::Permanent,
                "TEAMS_OPERATION_UNSUPPORTED",
            )),
        }
    }

    fn payload(&self) -> &TeamsOutboundPayload {
        match self {
            Self::Attention(payload) | Self::Approval(payload) => payload,
        }
    }

    fn require_scope(&self, config: &TeamsConnectorConfig) -> Result<(), ConnectorCallError> {
        let payload = self.payload();
        if payload.tenant_id != config.tenant
            || payload.team_id != config.team
            || payload.channel_id != config.channel
        {
            return Err(connector_error(
                ConnectorCallErrorKind::Permanent,
                "TEAMS_SCOPE_MISMATCH",
            ));
        }
        Ok(())
    }

    fn message(
        &self,
        claim: &OutboundClaim,
    ) -> Result<TeamsGraphOutboundMessage, ConnectorCallError> {
        let payload = self.payload();
        let (kind, actions) = match self {
            Self::Attention(_) => (
                "attention",
                json!([{
                    "data": {
                        "action": "attention.acknowledge",
                        "expiresAtMillis": payload.expires_at_millis,
                        "interactionId": payload.interaction_id,
                        "kind": "attention",
                        "operationKey": claim.operation_key().digest(),
                    },
                    "title": "Acknowledge",
                    "type": "Action.Submit",
                }]),
            ),
            Self::Approval(_) => (
                "approval",
                json!([
                    {
                        "data": {
                            "action": "approval.approve",
                            "expiresAtMillis": payload.expires_at_millis,
                            "interactionId": payload.interaction_id,
                            "kind": "approval",
                            "operationKey": claim.operation_key().digest(),
                        },
                        "title": "Approve",
                        "type": "Action.Submit",
                    },
                    {
                        "data": {
                            "action": "approval.reject",
                            "expiresAtMillis": payload.expires_at_millis,
                            "interactionId": payload.interaction_id,
                            "kind": "approval",
                            "operationKey": claim.operation_key().digest(),
                        },
                        "title": "Reject",
                        "type": "Action.Submit",
                    }
                ]),
            ),
        };
        let body = serde_json::to_vec(&json!({
            "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
            "actions": actions,
            "body": [
                {
                    "text": payload.title,
                    "type": "TextBlock",
                    "weight": "Bolder",
                    "wrap": true,
                },
                {
                    "text": payload.body,
                    "type": "TextBlock",
                    "wrap": true,
                }
            ],
            "fallbackText": format!("WinWinCode {kind} notification"),
            "type": "AdaptiveCard",
            "version": "1.4",
        }))
        .map_err(|_| connector_error(ConnectorCallErrorKind::Permanent, "TEAMS_PAYLOAD_INVALID"))?;
        Ok(TeamsGraphOutboundMessage {
            tenant_id: payload.tenant_id.clone(),
            team_id: payload.team_id.clone(),
            channel_id: payload.channel_id.clone(),
            operation_key: claim.operation_key().digest().clone(),
            body,
        })
    }
}

fn parse_notification(payload: &[u8]) -> Result<ParsedNotification, IntegrationError> {
    let envelope: GraphNotificationEnvelope = serde_json::from_slice(payload)
        .map_err(|_| invalid("Microsoft Graph notification payload is invalid"))?;
    validate_optional_graph_text(envelope.odata_context.as_deref())?;
    let [item] = envelope.value.as_slice() else {
        return Err(invalid(
            "Microsoft Graph webhook batch must contain one notification",
        ));
    };
    if envelope.validation_tokens.len() != 1 {
        return Err(invalid("Microsoft Graph validation token is missing"));
    }
    validate_notification_item(item)?;
    Ok(ParsedNotification {
        item: clone_notification_item(item),
        validation_token: envelope.validation_tokens.into_iter().next(),
    })
}

fn clone_notification_item(value: &GraphNotificationItem) -> GraphNotificationItem {
    GraphNotificationItem {
        subscription_id: value.subscription_id.clone(),
        subscription_expiration_date_time: value.subscription_expiration_date_time.clone(),
        change_type: value.change_type.clone(),
        client_state: value.client_state.clone(),
        tenant_id: value.tenant_id.clone(),
        resource: value.resource.clone(),
        sequence_number: value.sequence_number,
        resource_data: GraphResourceData {
            odata_id: value.resource_data.odata_id.clone(),
            odata_type: value.resource_data.odata_type.clone(),
            id: value.resource_data.id.clone(),
            team_id: value.resource_data.team_id.clone(),
            channel_id: value.resource_data.channel_id.clone(),
            from_user_id: value.resource_data.from_user_id.clone(),
            action: value.resource_data.action.clone(),
            interaction_id: value.resource_data.interaction_id.clone(),
            expires_at_millis: value.resource_data.expires_at_millis,
        },
        lifecycle_event: value.lifecycle_event.clone(),
    }
}

fn validate_notification_item(item: &GraphNotificationItem) -> Result<(), IntegrationError> {
    validate_graph_id(&item.subscription_id)?;
    validate_graph_id(&item.resource_data.id)?;
    validate_graph_id(&item.resource_data.from_user_id)?;
    validate_graph_id(&item.resource_data.interaction_id)?;
    validate_plain_token(&item.client_state)?;
    validate_optional_graph_text(item.subscription_expiration_date_time.as_deref())?;
    validate_optional_graph_text(item.lifecycle_event.as_deref())?;
    validate_optional_graph_text(item.resource_data.odata_id.as_deref())?;
    validate_optional_graph_text(item.resource_data.odata_type.as_deref())?;
    if item.sequence_number == 0
        || item.sequence_number > MAX_SAFE_INTEGER
        || item.resource_data.expires_at_millis == 0
        || item.resource_data.expires_at_millis > MAX_SAFE_INTEGER
        || !matches!(item.change_type.as_str(), "created" | "updated")
        || !matches!(
            item.resource_data.action.as_str(),
            "attention.acknowledge" | "approval.approve" | "approval.reject"
        )
    {
        return Err(invalid("Microsoft Graph notification facts are invalid"));
    }
    Ok(())
}

fn validate_optional_graph_text(value: Option<&str>) -> Result<(), IntegrationError> {
    if value.is_some_and(|value| {
        value.is_empty() || value.len() > 2_048 || value.bytes().any(|byte| byte.is_ascii_control())
    }) {
        Err(invalid("Microsoft Graph notification metadata is invalid"))
    } else {
        Ok(())
    }
}

fn require_notification_scope(
    config: &TeamsConnectorConfig,
    notification: &ParsedNotification,
) -> Result<(), IntegrationError> {
    let item = &notification.item;
    let expected_resource = format!(
        "teams/{}/channels/{}/messages/{}",
        config.team.as_str(),
        config.channel.as_str(),
        item.resource_data.id
    );
    let odata_id_matches = item
        .resource_data
        .odata_id
        .as_deref()
        .is_none_or(|value| value == expected_resource);
    let odata_type_matches = item
        .resource_data
        .odata_type
        .as_deref()
        .is_none_or(|value| value == "#Microsoft.Graph.chatMessage");
    if item.tenant_id != config.tenant
        || item.resource_data.team_id != config.team
        || item.resource_data.channel_id != config.channel
        || item.resource != expected_resource
        || !odata_id_matches
        || !odata_type_matches
    {
        return Err(invalid(
            "Microsoft Graph notification scope does not match connector",
        ));
    }
    Ok(())
}

fn normalize_notification(
    context: &InboundNormalizationContext,
    item: &GraphNotificationItem,
) -> Result<NormalizedInboundEvent, ConnectorCallError> {
    let disposition = if context.received_at_millis() > item.resource_data.expires_at_millis {
        "expired"
    } else {
        "active"
    };
    let payload = serde_json::to_vec(&json!({
        "action": item.resource_data.action,
        "channelId": item.resource_data.channel_id,
        "disposition": disposition,
        "eventKey": context.event_key(),
        "interactionId": item.resource_data.interaction_id,
        "providerSequence": context.provider_sequence(),
        "teamId": item.resource_data.team_id,
        "tenantId": item.tenant_id,
        "userId": item.resource_data.from_user_id,
    }))
    .map_err(|_| connector_error(ConnectorCallErrorKind::Permanent, "TEAMS_PAYLOAD_INVALID"))?;
    NormalizedInboundEvent::try_new("teams.interaction.handle", payload)
        .map_err(|_| connector_error(ConnectorCallErrorKind::Permanent, "TEAMS_PAYLOAD_INVALID"))
}

fn validate_outbound_payload(payload: &TeamsOutboundPayload) -> Result<(), ConnectorCallError> {
    if payload.interaction_id.is_empty()
        || payload.interaction_id.len() > MAX_OPAQUE_ID_BYTES
        || payload.title.is_empty()
        || payload.title.len() > MAX_MESSAGE_TEXT_BYTES
        || payload.body.is_empty()
        || payload.body.len() > MAX_MESSAGE_TEXT_BYTES
        || payload.expires_at_millis == 0
        || payload.expires_at_millis > MAX_SAFE_INTEGER
        || payload
            .title
            .bytes()
            .chain(payload.body.bytes())
            .any(|byte| byte == 0)
    {
        return Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "TEAMS_PAYLOAD_INVALID",
        ));
    }
    Ok(())
}

fn notification_external_id(item: &GraphNotificationItem) -> String {
    format!(
        "{}:{}:{}",
        item.subscription_id, item.resource_data.id, item.sequence_number
    )
}

fn matches_authority(config: &TeamsConnectorConfig, authority: &ConnectorAuthority) -> bool {
    authority.integration_id() == &config.integration
        && authority.protocol().as_str() == MICROSOFT_TEAMS_CONNECTOR_PROTOCOL
        && authority.credential_reference_id() == &config.credential_reference
}

fn require_authority(
    config: &TeamsConnectorConfig,
    authority: &ConnectorAuthority,
) -> Result<(), ConnectorCallError> {
    if matches_authority(config, authority) {
        Ok(())
    } else {
        Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "TEAMS_AUTHORITY_MISMATCH",
        ))
    }
}

fn signature_credential_error(error: TeamsCredentialError) -> SignatureVerificationError {
    match error.kind() {
        TeamsCredentialErrorKind::Unavailable => SignatureVerificationError::rejected(),
        TeamsCredentialErrorKind::Revoked => SignatureVerificationError::credential_revoked(),
    }
}

fn connector_credential_error(error: TeamsCredentialError) -> ConnectorCallError {
    let kind = match error.kind() {
        TeamsCredentialErrorKind::Unavailable => ConnectorCallErrorKind::Retryable,
        TeamsCredentialErrorKind::Revoked => ConnectorCallErrorKind::CredentialRevoked,
    };
    connector_error(kind, "TEAMS_CREDENTIAL_UNAVAILABLE")
}

fn graph_call_error(error: TeamsGraphCallError) -> ConnectorCallError {
    match error.kind() {
        TeamsGraphCallErrorKind::RateLimited => error
            .retry_after_millis()
            .and_then(|delay| ConnectorCallError::retryable_after("TEAMS_RATE_LIMITED", delay).ok())
            .unwrap_or_else(|| {
                connector_error(ConnectorCallErrorKind::Retryable, "TEAMS_RATE_LIMITED")
            }),
        TeamsGraphCallErrorKind::Retryable => connector_error(
            ConnectorCallErrorKind::Retryable,
            "TEAMS_SERVICE_UNAVAILABLE",
        ),
        TeamsGraphCallErrorKind::Permanent => {
            connector_error(ConnectorCallErrorKind::Permanent, "TEAMS_REQUEST_REJECTED")
        }
        TeamsGraphCallErrorKind::CredentialRevoked => connector_error(
            ConnectorCallErrorKind::CredentialRevoked,
            "TEAMS_CREDENTIAL_REVOKED",
        ),
    }
}

fn require_graph_success(
    response: &TeamsGraphHttpResponse,
    accepted: &[u16],
) -> Result<(), TeamsGraphCallError> {
    if accepted.contains(&response.status) {
        return Ok(());
    }
    if response.status == 429 {
        let delay = response
            .retry_after_seconds
            .and_then(|seconds| seconds.checked_mul(1_000))
            .ok_or_else(|| graph_error(TeamsGraphCallErrorKind::Retryable))?;
        return match TeamsGraphCallError::rate_limited(delay) {
            Ok(error) => Err(error),
            Err(_) => Err(graph_error(TeamsGraphCallErrorKind::Retryable)),
        };
    }
    let kind = match response.status {
        401 => TeamsGraphCallErrorKind::CredentialRevoked,
        408 | 409 | 425 | 500..=599 => TeamsGraphCallErrorKind::Retryable,
        _ => TeamsGraphCallErrorKind::Permanent,
    };
    Err(graph_error(kind))
}

fn graph_error(kind: TeamsGraphCallErrorKind) -> TeamsGraphCallError {
    TeamsGraphCallError::try_new(kind)
        .expect("non-rate-limit Microsoft Graph error kind must be valid")
}

fn graph_response_invalid() -> TeamsGraphCallError {
    graph_error(TeamsGraphCallErrorKind::Retryable)
}

fn operation_marker(operation_key: &Sha256Digest) -> String {
    format!("<!-- winwincode-operation:{} -->", operation_key.0)
}

fn client_request_id(operation_key: &Sha256Digest) -> String {
    let digest = Sha256::digest(operation_key.0.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut value = String::with_capacity(36);
    for (index, byte) in bytes.into_iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            value.push('-');
        }
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn canonical_api_base_url(value: &str) -> Result<String, IntegrationError> {
    canonical_absolute_url(value)?;
    if value.contains(['?', '#']) {
        return Err(invalid("Microsoft Graph API base URL is invalid"));
    }
    Ok(format!("{}/", value.trim_end_matches('/')))
}

fn canonical_absolute_url(value: &str) -> Result<(), IntegrationError> {
    let uri = ureq::http::Uri::from_str(value)
        .map_err(|_| invalid("Microsoft Graph API URL is invalid"))?;
    if value.len() > 4_096
        || value.contains('#')
        || uri.scheme_str() != Some("https")
        || uri.authority().is_none()
        || uri
            .authority()
            .is_some_and(|authority| authority.as_str().contains('@'))
    {
        return Err(invalid("Microsoft Graph API URL is invalid"));
    }
    Ok(())
}

fn validate_tls_roots(value: &TeamsGraphTlsRoots) -> Result<(), IntegrationError> {
    if let TeamsGraphTlsRoots::Specific(values) = value
        && (values.is_empty() || values.iter().any(Vec::is_empty))
    {
        return Err(invalid("Microsoft Graph TLS roots are invalid"));
    }
    Ok(())
}

fn encode_path_segment(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            output.push('%');
            output.push(char::from(b"0123456789ABCDEF"[usize::from(byte >> 4)]));
            output.push(char::from(b"0123456789ABCDEF"[usize::from(byte & 0x0f)]));
        }
    }
    output
}

fn remote_receipt(
    remote: &TeamsGraphMessageReceipt,
) -> Result<OutboundCallReceipt, ConnectorCallError> {
    let mut hash = Sha256::new();
    hash.update(b"winwincode.teams.remote-receipt.v1");
    hash.update([0]);
    hash.update(remote.message_id.as_bytes());
    OutboundCallReceipt::try_new(
        Sha256Digest(format!("sha256:{:x}", hash.finalize())),
        remote.remote_write_performed,
    )
    .map_err(|_| connector_error(ConnectorCallErrorKind::Permanent, "TEAMS_RESPONSE_INVALID"))
}

fn connector_error(kind: ConnectorCallErrorKind, code: &'static str) -> ConnectorCallError {
    ConnectorCallError::try_new(kind, code).expect("static Teams connector code must be valid")
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let longest = left.len().max(right.len());
    for index in 0..longest {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

fn validate_graph_id(value: &str) -> Result<(), IntegrationError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_OPAQUE_ID_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'@' | b'.')
        });
    if valid {
        Ok(())
    } else {
        Err(invalid("Microsoft Graph identity is invalid"))
    }
}

fn validate_plain_token(value: &str) -> Result<(), IntegrationError> {
    if value.is_empty()
        || value.len() > MAX_VALIDATION_TOKEN_BYTES
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        Err(invalid("Microsoft Graph validation token is invalid"))
    } else {
        Ok(())
    }
}

fn validate_prefixed_id(value: &str, prefix: &str) -> Result<(), IntegrationError> {
    let Some(tail) = value.strip_prefix(&format!("{prefix}_")) else {
        return Err(invalid("connector identity is invalid"));
    };
    if tail.len() == 26
        && tail.bytes().all(|byte| {
            matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
        })
    {
        Ok(())
    } else {
        Err(invalid("connector identity is invalid"))
    }
}

fn is_lowercase_guid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }
        })
}

fn percent_decode(value: &str) -> Result<String, IntegrationError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let Some(pair) = bytes.get(index + 1..index + 3) else {
                    return Err(invalid(
                        "Microsoft Graph validation token encoding is invalid",
                    ));
                };
                let high = hex_value(pair[0])?;
                let low = hex_value(pair[1])?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| invalid("Microsoft Graph validation token encoding is invalid"))
}

fn hex_value(value: u8) -> Result<u8, IntegrationError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(invalid(
            "Microsoft Graph validation token encoding is invalid",
        )),
    }
}

fn invalid(message: &'static str) -> IntegrationError {
    IntegrationError::new(IntegrationErrorKind::Invalid, message)
}
