// SPDX-License-Identifier: Apache-2.0

//! Slack protocol adapter over the durable Integration Framework.

use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
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

/// Canonical Integration Framework protocol identifier for Slack.
pub const SLACK_CONNECTOR_PROTOCOL: &str = "slack.web-api.v1";

const MAX_SIGNING_SECRET_BYTES: usize = 4_096;
const MAX_BOT_TOKEN_BYTES: usize = 16_384;
const MAX_FORM_BYTES: usize = 1_048_576;
const MAX_HEADER_CHARACTERS: usize = 150;
const MAX_SECTION_CHARACTERS: usize = 3_000;
const MAX_MESSAGE_BLOCKS: usize = 50;
const MAX_RESPONSE_BYTES: u64 = 2 * 1_024 * 1_024;
const SIGNATURE_WINDOW_SECONDS: u64 = 300;
const INTERACTION_ACK_BUDGET_MILLIS: u64 = 3_000;
const HISTORY_PAGE_SIZE: usize = 100;
const USER_AGENT: &str = "WinWinCode-Slack-Enterprise-Connector";
const RATE_LIMIT_SCHEMA: &str = r"
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
CREATE TABLE IF NOT EXISTS slack_method_rate_limits (
    workspace_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    method TEXT NOT NULL CHECK (method IN ('chat.postMessage', 'conversations.history')),
    blocked_until_millis INTEGER NOT NULL CHECK (blocked_until_millis > 0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    PRIMARY KEY (workspace_id, app_id, method)
);
";

/// Canonical Slack workspace identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SlackWorkspaceId(String);

impl SlackWorkspaceId {
    /// Builds a canonical Slack workspace identity.
    ///
    /// # Errors
    ///
    /// Rejects a non-portable Team or Enterprise identifier.
    pub fn try_new(value: impl Into<String>) -> Result<Self, IntegrationError> {
        let value = value.into();
        if !valid_slack_id(&value, b"TE") {
            return Err(invalid("Slack workspace identity is invalid"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical Slack application identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SlackAppId(String);

impl SlackAppId {
    /// Builds a canonical Slack application identity.
    ///
    /// # Errors
    ///
    /// Rejects a non-portable application identifier.
    pub fn try_new(value: impl Into<String>) -> Result<Self, IntegrationError> {
        let value = value.into();
        if !valid_slack_id(&value, b"A") {
            return Err(invalid("Slack application identity is invalid"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical Slack bot identity returned by `auth.test`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SlackBotId(String);

impl SlackBotId {
    /// Builds a canonical Slack bot identity.
    ///
    /// # Errors
    ///
    /// Rejects a non-portable bot identifier.
    pub fn try_new(value: impl Into<String>) -> Result<Self, IntegrationError> {
        let value = value.into();
        if !valid_slack_id(&value, b"B") {
            return Err(invalid("Slack bot identity is invalid"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exact non-secret Slack installation identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlackInstallationIdentity {
    workspace: SlackWorkspaceId,
    app: SlackAppId,
    bot: SlackBotId,
}

impl SlackInstallationIdentity {
    #[must_use]
    pub const fn new(workspace: SlackWorkspaceId, app: SlackAppId, bot: SlackBotId) -> Self {
        Self {
            workspace,
            app,
            bot,
        }
    }

    #[must_use]
    pub const fn workspace_id(&self) -> &SlackWorkspaceId {
        &self.workspace
    }

    #[must_use]
    pub const fn app_id(&self) -> &SlackAppId {
        &self.app
    }

    #[must_use]
    pub const fn bot_id(&self) -> &SlackBotId {
        &self.bot
    }
}

/// Canonical Slack conversation identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SlackChannelId(String);

impl SlackChannelId {
    /// Builds a canonical Slack public/private/direct conversation identity.
    ///
    /// # Errors
    ///
    /// Rejects a non-portable conversation identifier.
    pub fn try_new(value: impl Into<String>) -> Result<Self, IntegrationError> {
        let value = value.into();
        if !valid_slack_id(&value, b"CGD") {
            return Err(invalid("Slack channel identity is invalid"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Explicit TLS roots for Slack or a loopback Slack sandbox.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SlackTlsRoots {
    WebPki,
    Specific(Vec<Vec<u8>>),
}

/// Credential-free connector authority for one workspace/channel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlackConnectorConfig {
    integration: EnterpriseIntegrationId,
    credential_reference: CredentialReferenceId,
    installation: SlackInstallationIdentity,
    channel: SlackChannelId,
    api_base_url: String,
    control_plane_base_url: String,
    tls_roots: SlackTlsRoots,
    request_timeout: Duration,
    max_lookup_pages: u16,
}

impl SlackConnectorConfig {
    /// Builds one exact Slack workspace/channel and Control Plane deep-link boundary.
    ///
    /// # Errors
    ///
    /// Rejects invalid identities, insecure URLs, or malformed TLS roots.
    pub fn try_new(
        integration_id: EnterpriseIntegrationId,
        credential_reference_id: CredentialReferenceId,
        installation: SlackInstallationIdentity,
        channel_id: SlackChannelId,
        api_base_url: impl Into<String>,
        control_plane_base_url: impl Into<String>,
        tls_roots: SlackTlsRoots,
    ) -> Result<Self, IntegrationError> {
        validate_integration_id(&integration_id)?;
        validate_prefixed_id(&credential_reference_id.0, "crd")?;
        validate_tls_roots(&tls_roots)?;
        Ok(Self {
            integration: integration_id,
            credential_reference: credential_reference_id,
            installation,
            channel: channel_id,
            api_base_url: canonical_base_url(&api_base_url.into(), "Slack API")?,
            control_plane_base_url: canonical_base_url(
                &control_plane_base_url.into(),
                "Control Plane",
            )?,
            tls_roots,
            request_timeout: Duration::from_secs(30),
            max_lookup_pages: 20,
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
    pub const fn workspace_id(&self) -> &SlackWorkspaceId {
        self.installation.workspace_id()
    }

    #[must_use]
    pub const fn installation(&self) -> &SlackInstallationIdentity {
        &self.installation
    }

    #[must_use]
    pub const fn channel_id(&self) -> &SlackChannelId {
        &self.channel
    }
}

/// Slack request signature headers preserved only for immediate verification.
#[derive(Clone, Eq, PartialEq)]
pub struct SlackWebhookHeaders {
    proof: SlackSigningProof,
}

impl SlackWebhookHeaders {
    /// Builds canonical Slack signing headers.
    ///
    /// # Errors
    ///
    /// Rejects unsafe time or a non-canonical `v0=` signature.
    pub fn try_new(
        timestamp_seconds: u64,
        signature: impl Into<String>,
    ) -> Result<Self, IntegrationError> {
        let signature = signature.into();
        if timestamp_seconds == 0
            || timestamp_seconds > MAX_SAFE_INTEGER
            || decode_signature(signature.as_bytes()).is_none()
        {
            return Err(invalid("Slack signing headers are invalid"));
        }
        Ok(Self {
            proof: SlackSigningProof {
                timestamp_seconds,
                signature,
            },
        })
    }
}

impl fmt::Debug for SlackWebhookHeaders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SlackWebhookHeaders")
            .field("proof", &self.proof)
            .finish()
    }
}

/// Immediate empty acknowledgment for one valid Slack interaction request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlackInteractionAcknowledgement {
    send_by_millis: u64,
}

impl SlackInteractionAcknowledgement {
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        200
    }

    #[must_use]
    pub const fn body(&self) -> &[u8] {
        &[]
    }

    #[must_use]
    pub const fn send_by_millis(&self) -> u64 {
        self.send_by_millis
    }
}

/// Parsed Slack interaction plus its mandatory immediate acknowledgment.
pub struct SlackInteractionIngress {
    acknowledgement: SlackInteractionAcknowledgement,
    decision: Option<InboundWebhookRequest>,
}

impl fmt::Debug for SlackInteractionIngress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SlackInteractionIngress")
            .field("acknowledgement", &self.acknowledgement)
            .field("has_decision_request", &self.decision.is_some())
            .finish()
    }
}

impl SlackInteractionIngress {
    #[must_use]
    pub const fn acknowledgement(&self) -> &SlackInteractionAcknowledgement {
        &self.acknowledgement
    }

    #[must_use]
    pub const fn decision_request(&self) -> Option<&InboundWebhookRequest> {
        self.decision.as_ref()
    }

    #[must_use]
    pub fn into_decision_request(self) -> Option<InboundWebhookRequest> {
        self.decision
    }
}

/// Builds Integration Framework requests from Slack interactivity form bodies.
#[derive(Clone, Debug)]
pub struct SlackWebhookRequestFactory {
    config: SlackConnectorConfig,
}

impl SlackWebhookRequestFactory {
    #[must_use]
    pub const fn new(config: SlackConnectorConfig) -> Self {
        Self { config }
    }

    /// Builds one exact raw request after closed callback/scope validation.
    ///
    /// # Errors
    ///
    /// Rejects malformed form data, unsupported actions, or foreign workspace/channel scope.
    pub fn accept(
        &self,
        scope: AuditScope,
        headers: &SlackWebhookHeaders,
        raw_form_body: Vec<u8>,
        received_at_millis: u64,
    ) -> Result<SlackInteractionIngress, IntegrationError> {
        let interaction = parse_interaction(&raw_form_body)?;
        require_interaction_scope(&self.config, &interaction)?;
        let action = single_action(&interaction)?;
        let send_by_millis = (received_at_millis > 0)
            .then_some(received_at_millis)
            .and_then(|value| {
                value
                    .checked_add(INTERACTION_ACK_BUDGET_MILLIS)
                    .filter(|value| *value <= MAX_SAFE_INTEGER)
            })
            .ok_or_else(|| invalid("Slack acknowledgment time is invalid"))?;
        let acknowledgement = SlackInteractionAcknowledgement { send_by_millis };
        if action.action_id == "control-plane.open" {
            require_open_action(action)?;
            return Ok(SlackInteractionIngress {
                acknowledgement,
                decision: None,
            });
        }
        let action_value = parse_action_value(action)?;
        let sequence = parse_slack_sequence(&action.action_ts)?;
        let external_id = callback_external_id(&interaction, action, &action_value);
        let metadata = InboundWebhookMetadata::try_new(
            "slack.block_actions",
            external_id,
            action_value.interaction_id.clone(),
            sequence,
            received_at_millis,
        )?;
        let proof = serde_json::to_vec(&headers.proof)
            .map_err(|_| invalid("Slack signature proof is invalid"))?;
        let decision = InboundWebhookRequest::try_new(
            self.config.integration.clone(),
            scope,
            metadata,
            proof,
            raw_form_body,
        )?;
        Ok(SlackInteractionIngress {
            acknowledgement,
            decision: Some(decision),
        })
    }
}

/// Secret Slack app signing material.
pub struct SlackSigningSecret(Vec<u8>);

impl SlackSigningSecret {
    /// Builds a bounded non-empty signing secret.
    ///
    /// # Errors
    ///
    /// Rejects empty or oversized material.
    pub fn try_new(value: impl AsRef<[u8]>) -> Result<Self, IntegrationError> {
        let value = value.as_ref();
        if value.is_empty() || value.len() > MAX_SIGNING_SECRET_BYTES {
            return Err(invalid("Slack signing secret is invalid"));
        }
        Ok(Self(value.to_vec()))
    }

    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SlackSigningSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SlackSigningSecret([REDACTED])")
    }
}

impl Drop for SlackSigningSecret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Closed Slack token permission snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlackBotPermissions {
    chat_write: bool,
    history_read: bool,
}

impl SlackBotPermissions {
    #[must_use]
    pub const fn new(chat_write: bool, history_read: bool) -> Self {
        Self {
            chat_write,
            history_read,
        }
    }
}

/// Short-lived, scope-bound Slack bot token.
pub struct SlackBotToken {
    value: Vec<u8>,
    installation: SlackInstallationIdentity,
    channel: SlackChannelId,
    permissions: SlackBotPermissions,
}

impl SlackBotToken {
    /// Builds a token bound to its resolved workspace/channel permission facts.
    ///
    /// # Errors
    ///
    /// Rejects empty, control-containing, or oversized token material.
    pub fn try_new(
        value: impl Into<String>,
        installation: SlackInstallationIdentity,
        channel_id: SlackChannelId,
        permissions: SlackBotPermissions,
    ) -> Result<Self, IntegrationError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_BOT_TOKEN_BYTES
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(invalid("Slack bot token is invalid"));
        }
        Ok(Self {
            value: value.into_bytes(),
            installation,
            channel: channel_id,
            permissions,
        })
    }

    fn value(&self) -> Result<&str, ConnectorCallError> {
        std::str::from_utf8(&self.value).map_err(|_| {
            connector_error(
                ConnectorCallErrorKind::CredentialRevoked,
                "SLACK_CREDENTIAL_REVOKED",
            )
        })
    }
}

impl fmt::Debug for SlackBotToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SlackBotToken")
            .field("value", &"[REDACTED]")
            .field("installation", &self.installation)
            .field("channel", &self.channel)
            .field("permissions", &self.permissions)
            .finish()
    }
}

impl Drop for SlackBotToken {
    fn drop(&mut self) {
        self.value.fill(0);
    }
}

/// Credential lookup failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlackCredentialErrorKind {
    Unavailable,
    Revoked,
}

/// Secret-safe Slack credential resolution failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlackCredentialError(SlackCredentialErrorKind);

impl SlackCredentialError {
    #[must_use]
    pub const fn unavailable() -> Self {
        Self(SlackCredentialErrorKind::Unavailable)
    }

    #[must_use]
    pub const fn revoked() -> Self {
        Self(SlackCredentialErrorKind::Revoked)
    }

    #[must_use]
    pub const fn kind(self) -> SlackCredentialErrorKind {
        self.0
    }
}

impl fmt::Display for SlackCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Slack credential lookup failed")
    }
}

impl std::error::Error for SlackCredentialError {}

/// Credential authority used by signing verification and Web API calls.
pub trait SlackCredentialPort {
    /// Resolves the Slack app signing secret.
    ///
    /// # Errors
    ///
    /// Returns only stable missing/revoked facts.
    fn resolve_signing_secret(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<SlackSigningSecret, SlackCredentialError>;

    /// Resolves a scope-bound bot token.
    ///
    /// # Errors
    ///
    /// Returns only stable missing/revoked facts.
    fn resolve_bot_token(
        &mut self,
        reference: &CredentialReferenceId,
        installation: &SlackInstallationIdentity,
    ) -> Result<SlackBotToken, SlackCredentialError>;
}

/// Clock used for Slack signature and durable rate-limit boundaries.
pub trait SlackClock {
    #[must_use]
    fn now_millis(&self) -> u64;
}

/// Production wall clock for Slack protocol boundaries.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSlackClock;

impl SlackClock for SystemSlackClock {
    fn now_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .unwrap_or(MAX_SAFE_INTEGER)
    }
}

/// Slack HMAC-SHA256 verifier with strict timestamp replay protection.
pub struct SlackWebhookVerifier<Credentials, Clock> {
    config: SlackConnectorConfig,
    credentials: Credentials,
    clock: Clock,
}

impl<Credentials, Clock> SlackWebhookVerifier<Credentials, Clock> {
    #[must_use]
    pub const fn new(config: SlackConnectorConfig, credentials: Credentials, clock: Clock) -> Self {
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

impl<Credentials: SlackCredentialPort, Clock: SlackClock> WebhookSignatureVerifier
    for SlackWebhookVerifier<Credentials, Clock>
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
        let proof: SlackSigningProof = serde_json::from_slice(signature)
            .map_err(|_| SignatureVerificationError::rejected())?;
        let supplied = decode_signature(proof.signature.as_bytes())
            .ok_or_else(SignatureVerificationError::rejected)?;
        let now = self.clock.now_millis() / 1_000;
        if now.abs_diff(proof.timestamp_seconds) > SIGNATURE_WINDOW_SECONDS {
            return Err(SignatureVerificationError::rejected());
        }
        let secret = self
            .credentials
            .resolve_signing_secret(authority.credential_reference_id())
            .map_err(signature_credential_error)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.bytes())
            .map_err(|_| SignatureVerificationError::rejected())?;
        mac.update(b"v0:");
        mac.update(proof.timestamp_seconds.to_string().as_bytes());
        mac.update(b":");
        mac.update(payload);
        mac.verify_slice(&supplied)
            .map_err(|_| SignatureVerificationError::rejected())
    }
}

/// Slack Web API methods with independent workspace/application rate gates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlackWebApiMethod {
    ChatPostMessage,
    ConversationsHistory,
}

impl SlackWebApiMethod {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ChatPostMessage => "chat.postMessage",
            Self::ConversationsHistory => "conversations.history",
        }
    }
}

/// Durable Slack `Retry-After` floor shared by connector instances.
#[derive(Clone, Debug)]
pub struct SlackRateLimitGate {
    database_path: PathBuf,
}

impl SlackRateLimitGate {
    /// Opens the Slack rate-limit database below one private integration directory.
    ///
    /// # Errors
    ///
    /// Returns a stable adapter error when the directory, database, or schema is unavailable.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, IntegrationError> {
        let data_directory = data_directory.as_ref();
        fs::create_dir_all(data_directory).map_err(|_| slack_storage_error())?;
        let database_path = data_directory.join("slack-rate-limits.sqlite3");
        let connection =
            open_rate_limit_connection(&database_path).map_err(|_| slack_storage_error())?;
        connection
            .execute_batch(RATE_LIMIT_SCHEMA)
            .map_err(|_| slack_storage_error())?;
        Ok(Self { database_path })
    }

    /// Returns the current shared lower-bound delay for one workspace/application/method.
    ///
    /// # Errors
    ///
    /// Fails closed when the clock or durable gate is invalid.
    pub fn retry_after_millis(
        &self,
        installation: &SlackInstallationIdentity,
        method: SlackWebApiMethod,
        now_millis: u64,
    ) -> Result<Option<u64>, ConnectorCallError> {
        validate_slack_time(now_millis)?;
        let connection = open_rate_limit_connection(&self.database_path)
            .map_err(|_| rate_limit_storage_error())?;
        let blocked_until: Option<i64> = connection
            .query_row(
                "SELECT blocked_until_millis FROM slack_method_rate_limits
                 WHERE workspace_id = ?1 AND app_id = ?2 AND method = ?3",
                params![
                    installation.workspace_id().as_str(),
                    installation.app_id().as_str(),
                    method.as_str()
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| rate_limit_storage_error())?;
        blocked_until
            .map(from_sql_millis)
            .transpose()
            .map(|value| value.filter(|blocked_until| *blocked_until > now_millis))
            .map(|value| value.map(|blocked_until| blocked_until - now_millis))
    }

    fn observe(
        &self,
        installation: &SlackInstallationIdentity,
        method: SlackWebApiMethod,
        now_millis: u64,
        retry_after_millis: u64,
    ) -> Result<u64, ConnectorCallError> {
        validate_slack_time(now_millis)?;
        let proposed = now_millis
            .checked_add(retry_after_millis)
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .ok_or_else(rate_limit_storage_error)?;
        let mut connection = open_rate_limit_connection(&self.database_path)
            .map_err(|_| rate_limit_storage_error())?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| rate_limit_storage_error())?;
        let existing: Option<(i64, i64)> = transaction
            .query_row(
                "SELECT blocked_until_millis, revision FROM slack_method_rate_limits
                 WHERE workspace_id = ?1 AND app_id = ?2 AND method = ?3",
                params![
                    installation.workspace_id().as_str(),
                    installation.app_id().as_str(),
                    method.as_str()
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| rate_limit_storage_error())?;
        let (blocked_until, revision) =
            existing.map_or(Ok((proposed, 1_u64)), |(stored_until, stored_revision)| {
                let stored_until = from_sql_millis(stored_until)?;
                let stored_revision = from_sql_millis(stored_revision)?;
                Ok((
                    proposed.max(stored_until),
                    stored_revision
                        .checked_add(1)
                        .filter(|value| *value <= MAX_SAFE_INTEGER)
                        .ok_or_else(rate_limit_storage_error)?,
                ))
            })?;
        transaction
            .execute(
                "INSERT INTO slack_method_rate_limits
                 (workspace_id, app_id, method, blocked_until_millis, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(workspace_id, app_id, method) DO UPDATE SET
                   blocked_until_millis = excluded.blocked_until_millis,
                   revision = excluded.revision",
                params![
                    installation.workspace_id().as_str(),
                    installation.app_id().as_str(),
                    method.as_str(),
                    to_sql_millis(blocked_until)?,
                    to_sql_millis(revision)?
                ],
            )
            .map_err(|_| rate_limit_storage_error())?;
        transaction
            .commit()
            .map_err(|_| rate_limit_storage_error())?;
        Ok(blocked_until - now_millis)
    }
}

/// Slack inbound normalizer and retry-stable Web API adapter.
pub struct SlackEnterpriseConnector<Credentials, Clock> {
    config: SlackConnectorConfig,
    credentials: Credentials,
    rate_limits: SlackRateLimitGate,
    clock: Clock,
    agent: ureq::Agent,
}

impl<Credentials, Clock> SlackEnterpriseConnector<Credentials, Clock> {
    /// Builds a no-proxy, no-redirect, rustls-verified Slack connector.
    ///
    /// # Errors
    ///
    /// Rejects malformed explicit TLS roots.
    pub fn try_new(
        config: SlackConnectorConfig,
        credentials: Credentials,
        rate_limits: SlackRateLimitGate,
        clock: Clock,
    ) -> Result<Self, IntegrationError> {
        let roots = match &config.tls_roots {
            SlackTlsRoots::WebPki => ureq::tls::RootCerts::WebPki,
            SlackTlsRoots::Specific(values) => values
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
            rate_limits,
            clock,
            agent,
        })
    }

    #[must_use]
    pub fn into_credentials(self) -> Credentials {
        self.credentials
    }
}

impl<Credentials: SlackCredentialPort, Clock: SlackClock> ConnectorPort
    for SlackEnterpriseConnector<Credentials, Clock>
{
    fn normalize_inbound(
        &mut self,
        authority: &ConnectorAuthority,
        context: &InboundNormalizationContext,
        payload: &[u8],
    ) -> Result<NormalizedInboundEvent, ConnectorCallError> {
        require_authority(&self.config, authority)?;
        if context.event_type() != "slack.block_actions" {
            return Err(connector_error(
                ConnectorCallErrorKind::Permanent,
                "SLACK_EVENT_UNSUPPORTED",
            ));
        }
        let interaction = parse_interaction(payload).map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "SLACK_PAYLOAD_INVALID")
        })?;
        require_interaction_scope(&self.config, &interaction).map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "SLACK_SCOPE_MISMATCH")
        })?;
        normalize_interaction(context, &interaction)
    }

    fn deliver_outbound(
        &mut self,
        claim: &OutboundClaim,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        require_authority(&self.config, claim.authority())?;
        let operation = SlackOutboundOperation::parse(claim.operation_name(), claim.payload())?;
        operation.require_scope(&self.config)?;
        let token = self
            .credentials
            .resolve_bot_token(
                claim.authority().credential_reference_id(),
                &self.config.installation,
            )
            .map_err(connector_credential_error)?;
        require_token(&self.config, &token)?;
        self.deliver_operation(claim, &token, &operation)
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SlackSigningProof {
    timestamp_seconds: u64,
    signature: String,
}

impl fmt::Debug for SlackSigningProof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SlackSigningProof")
            .field("timestamp_seconds", &self.timestamp_seconds)
            .field("signature", &"[REDACTED]")
            .finish()
    }
}

#[derive(Deserialize)]
struct SlackInteraction {
    #[serde(rename = "type")]
    kind: String,
    api_app_id: String,
    team: SlackIdentity,
    channel: SlackIdentity,
    user: SlackIdentity,
    container: SlackContainer,
    actions: Vec<SlackAction>,
}

#[derive(Deserialize)]
struct SlackIdentity {
    id: String,
}

#[derive(Deserialize)]
struct SlackContainer {
    channel_id: String,
    message_ts: String,
}

#[derive(Deserialize)]
struct SlackAction {
    action_id: String,
    action_ts: String,
    value: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SlackActionValue {
    action: String,
    interaction_id: String,
    expected_revision: u64,
    expires_at_millis: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SlackOutboundPayload {
    workspace_id: SlackWorkspaceId,
    channel_id: SlackChannelId,
    interaction_id: String,
    expected_revision: u64,
    expires_at_millis: u64,
    title: String,
    body: String,
}

enum SlackOutboundOperation {
    Attention(SlackOutboundPayload),
    Approval(SlackOutboundPayload),
}

impl SlackOutboundOperation {
    fn parse(name: &str, payload: &[u8]) -> Result<Self, ConnectorCallError> {
        let payload: SlackOutboundPayload = serde_json::from_slice(payload).map_err(|_| {
            connector_error(ConnectorCallErrorKind::Permanent, "SLACK_PAYLOAD_INVALID")
        })?;
        validate_outbound_payload(&payload)?;
        match name {
            "slack.attention.notify" => Ok(Self::Attention(payload)),
            "slack.approval.notify" => Ok(Self::Approval(payload)),
            _ => Err(connector_error(
                ConnectorCallErrorKind::Permanent,
                "SLACK_OPERATION_UNSUPPORTED",
            )),
        }
    }

    fn payload(&self) -> &SlackOutboundPayload {
        match self {
            Self::Attention(payload) | Self::Approval(payload) => payload,
        }
    }

    fn require_scope(&self, config: &SlackConnectorConfig) -> Result<(), ConnectorCallError> {
        let payload = self.payload();
        if &payload.workspace_id != config.workspace_id() || payload.channel_id != config.channel {
            return Err(connector_error(
                ConnectorCallErrorKind::Permanent,
                "SLACK_SCOPE_MISMATCH",
            ));
        }
        Ok(())
    }
}

struct SlackHttpResponse {
    status: u16,
    retry_after_seconds: Option<u64>,
    body: Option<Value>,
}

impl<Credentials: SlackCredentialPort, Clock: SlackClock>
    SlackEnterpriseConnector<Credentials, Clock>
{
    fn deliver_operation(
        &self,
        claim: &OutboundClaim,
        token: &SlackBotToken,
        operation: &SlackOutboundOperation,
    ) -> Result<OutboundCallReceipt, ConnectorCallError> {
        if let Some(remote_ts) = self.find_message(claim, token)? {
            return remote_receipt(&remote_ts, false);
        }
        let body = self.message_body(claim, operation)?;
        let response = self.request(
            token,
            SlackWebApiMethod::ChatPostMessage,
            &format!("{}chat.postMessage", self.config.api_base_url),
            Some(&body),
            claim.operation_key().digest(),
        )?;
        require_slack_success(&response)?;
        let body = response.body.as_ref().ok_or_else(response_invalid)?;
        if body.get("channel").and_then(Value::as_str) != Some(self.config.channel.as_str()) {
            return Err(response_invalid());
        }
        let remote_ts = body
            .get("ts")
            .and_then(Value::as_str)
            .filter(|value| valid_slack_timestamp(value))
            .ok_or_else(response_invalid)?;
        remote_receipt(remote_ts, true)
    }

    fn find_message(
        &self,
        claim: &OutboundClaim,
        token: &SlackBotToken,
    ) -> Result<Option<String>, ConnectorCallError> {
        let mut cursor: Option<String> = None;
        for _ in 0..self.config.max_lookup_pages {
            let mut url = format!(
                "{}conversations.history?channel={}&include_all_metadata=true&limit={HISTORY_PAGE_SIZE}",
                self.config.api_base_url,
                self.config.channel.as_str()
            );
            if let Some(cursor) = &cursor {
                let _ = write!(url, "&cursor={}", percent_encode(cursor));
            }
            let response = self.request(
                token,
                SlackWebApiMethod::ConversationsHistory,
                &url,
                None,
                claim.operation_key().digest(),
            )?;
            require_slack_success(&response)?;
            let body = response.body.as_ref().ok_or_else(response_invalid)?;
            let messages = body
                .get("messages")
                .and_then(Value::as_array)
                .ok_or_else(response_invalid)?;
            if let Some(timestamp) = messages.iter().find_map(|message| {
                message_matches_installation(message, &self.config.installation)
                    .then(|| operation_key_from_message(message))
                    .flatten()
                    .is_some_and(|key| key == claim.operation_key().digest().0)
                    .then(|| message.get("ts").and_then(Value::as_str))
                    .flatten()
                    .filter(|timestamp| valid_slack_timestamp(timestamp))
                    .map(str::to_owned)
            }) {
                return Ok(Some(timestamp));
            }
            let next_cursor = body
                .get("response_metadata")
                .and_then(|metadata| metadata.get("next_cursor"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if next_cursor.is_empty() {
                return Ok(None);
            }
            if next_cursor.len() > 2_048 || next_cursor.bytes().any(|byte| byte.is_ascii_control())
            {
                return Err(response_invalid());
            }
            cursor = Some(next_cursor.to_owned());
        }
        Err(connector_error(
            ConnectorCallErrorKind::Retryable,
            "SLACK_LOOKUP_BOUND_EXCEEDED",
        ))
    }

    fn message_body(
        &self,
        claim: &OutboundClaim,
        operation: &SlackOutboundOperation,
    ) -> Result<Value, ConnectorCallError> {
        let payload = operation.payload();
        let deep_link = format!(
            "{}interactions/{}",
            self.config.control_plane_base_url,
            percent_encode(&payload.interaction_id)
        );
        if deep_link.chars().count() > 3_000 {
            return Err(connector_error(
                ConnectorCallErrorKind::Permanent,
                "SLACK_PAYLOAD_INVALID",
            ));
        }
        let mut elements = Vec::new();
        match operation {
            SlackOutboundOperation::Attention(_) => elements.push(action_button(
                "Acknowledge",
                "attention.acknowledge",
                payload,
                None,
            )?),
            SlackOutboundOperation::Approval(_) => {
                elements.push(action_button(
                    "Approve",
                    "approval.approve",
                    payload,
                    Some("primary"),
                )?);
                elements.push(action_button(
                    "Reject",
                    "approval.reject",
                    payload,
                    Some("danger"),
                )?);
            }
        }
        elements.push(json!({
            "action_id": "control-plane.open",
            "text": {"text": "Open in WinWinCode", "type": "plain_text"},
            "type": "button",
            "url": deep_link,
            "value": payload.interaction_id,
        }));
        let blocks = vec![
            json!({
                "text": {"text": payload.title, "type": "plain_text"},
                "type": "header",
            }),
            json!({
                "text": {"text": payload.body, "type": "plain_text"},
                "type": "section",
            }),
            json!({"elements": elements, "type": "actions"}),
        ];
        if blocks.len() > MAX_MESSAGE_BLOCKS {
            return Err(connector_error(
                ConnectorCallErrorKind::Permanent,
                "SLACK_PAYLOAD_INVALID",
            ));
        }
        Ok(json!({
            "blocks": blocks,
            "channel": self.config.channel,
            "client_msg_id": stable_uuid(claim.operation_key().digest()),
            "metadata": {
                "event_payload": {
                    "app_id": self.config.installation.app_id(),
                    "bot_id": self.config.installation.bot_id(),
                    "operation_key": claim.operation_key().digest(),
                    "team_id": self.config.installation.workspace_id(),
                },
                "event_type": "winwincode_notification",
            },
            "text": payload.title,
        }))
    }

    fn request(
        &self,
        token: &SlackBotToken,
        method: SlackWebApiMethod,
        url: &str,
        body: Option<&Value>,
        operation_key: &Sha256Digest,
    ) -> Result<SlackHttpResponse, ConnectorCallError> {
        let now_millis = self.clock.now_millis();
        if let Some(delay) =
            self.rate_limits
                .retry_after_millis(&self.config.installation, method, now_millis)?
        {
            let error = ConnectorCallError::retryable_after("SLACK_RATE_LIMITED", delay)
                .map_err(|_| rate_limit_storage_error())?;
            return Err(error);
        }
        let authorization = format!("Bearer {}", token.value()?);
        let request_id = stable_uuid(operation_key);
        let response = match (method, body) {
            (SlackWebApiMethod::ConversationsHistory, None) => self
                .agent
                .get(url)
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .header("X-Slack-Request-Id", &request_id)
                .header("User-Agent", USER_AGENT)
                .call(),
            (SlackWebApiMethod::ChatPostMessage, Some(body)) => self
                .agent
                .post(url)
                .header("Accept", "application/json")
                .header("Authorization", &authorization)
                .header("X-Slack-Request-Id", &request_id)
                .header("User-Agent", USER_AGENT)
                .send_json(body),
            _ => {
                return Err(connector_error(
                    ConnectorCallErrorKind::Permanent,
                    "SLACK_REQUEST_INVALID",
                ));
            }
        }
        .map_err(|_| {
            connector_error(
                ConnectorCallErrorKind::Retryable,
                "SLACK_TRANSPORT_UNAVAILABLE",
            )
        })?;
        let status = response.status().as_u16();
        let retry_after_seconds = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if status == 429
            && let Some(retry_after_millis) = retry_after_seconds
                .filter(|seconds| *seconds > 0)
                .and_then(|seconds| seconds.checked_mul(1_000))
        {
            let _ = self.rate_limits.observe(
                &self.config.installation,
                method,
                now_millis,
                retry_after_millis,
            )?;
        }
        let bytes = response
            .into_body()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(|_| {
                connector_error(
                    ConnectorCallErrorKind::Retryable,
                    "SLACK_RESPONSE_UNREADABLE",
                )
            })?;
        let body = if bytes.is_empty() {
            None
        } else {
            serde_json::from_slice(&bytes).ok()
        };
        Ok(SlackHttpResponse {
            status,
            retry_after_seconds,
            body,
        })
    }
}

fn parse_interaction(raw_form_body: &[u8]) -> Result<SlackInteraction, IntegrationError> {
    if raw_form_body.is_empty() || raw_form_body.len() > MAX_FORM_BYTES {
        return Err(invalid("Slack interactivity form is invalid"));
    }
    let raw = std::str::from_utf8(raw_form_body)
        .map_err(|_| invalid("Slack interactivity form is invalid"))?;
    let mut payload: Option<String> = None;
    for pair in raw.split('&') {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| invalid("Slack interactivity form is invalid"))?;
        if key != "payload" || payload.is_some() {
            return Err(invalid("Slack interactivity form is invalid"));
        }
        payload = Some(form_decode(value)?);
    }
    let payload = payload.ok_or_else(|| invalid("Slack interactivity payload is missing"))?;
    serde_json::from_str(&payload).map_err(|_| invalid("Slack interactivity payload is invalid"))
}

fn require_interaction_scope(
    config: &SlackConnectorConfig,
    interaction: &SlackInteraction,
) -> Result<(), IntegrationError> {
    if interaction.kind != "block_actions"
        || interaction.team.id != config.installation.workspace_id().as_str()
        || interaction.api_app_id != config.installation.app_id().as_str()
        || interaction.channel.id != config.channel.as_str()
        || interaction.container.channel_id != config.channel.as_str()
        || !valid_slack_id(&interaction.user.id, b"UW")
        || !valid_slack_timestamp(&interaction.container.message_ts)
    {
        return Err(invalid("Slack interaction scope is invalid"));
    }
    Ok(())
}

fn require_open_action(action: &SlackAction) -> Result<(), IntegrationError> {
    if action.action_id != "control-plane.open"
        || !valid_portable_text(&action.value, 256)
        || !valid_slack_timestamp(&action.action_ts)
    {
        return Err(invalid("Slack open action facts are invalid"));
    }
    Ok(())
}

fn single_action(interaction: &SlackInteraction) -> Result<&SlackAction, IntegrationError> {
    let [action] = interaction.actions.as_slice() else {
        return Err(invalid("Slack interaction must contain one action"));
    };
    Ok(action)
}

fn parse_action_value(action: &SlackAction) -> Result<SlackActionValue, IntegrationError> {
    let value: SlackActionValue = serde_json::from_str(&action.value)
        .map_err(|_| invalid("Slack action value is invalid"))?;
    if action.action_id != value.action
        || !matches!(
            value.action.as_str(),
            "attention.acknowledge" | "approval.approve" | "approval.reject"
        )
        || !valid_portable_text(&value.interaction_id, 256)
        || value.expected_revision == 0
        || value.expected_revision > MAX_SAFE_INTEGER
        || value.expires_at_millis == 0
        || value.expires_at_millis > MAX_SAFE_INTEGER
    {
        return Err(invalid("Slack action facts are invalid"));
    }
    Ok(value)
}

fn normalize_interaction(
    context: &InboundNormalizationContext,
    interaction: &SlackInteraction,
) -> Result<NormalizedInboundEvent, ConnectorCallError> {
    let action = single_action(interaction)
        .map_err(|_| connector_error(ConnectorCallErrorKind::Permanent, "SLACK_PAYLOAD_INVALID"))?;
    let value = parse_action_value(action)
        .map_err(|_| connector_error(ConnectorCallErrorKind::Permanent, "SLACK_PAYLOAD_INVALID"))?;
    let disposition = if context.received_at_millis() > value.expires_at_millis {
        "expired"
    } else {
        "active"
    };
    let payload = serde_json::to_vec(&json!({
        "action": value.action,
        "channelId": interaction.channel.id,
        "disposition": disposition,
        "eventKey": context.event_key(),
        "expectedRevision": value.expected_revision,
        "expiresAtMillis": value.expires_at_millis,
        "interactionId": value.interaction_id,
        "messageTs": interaction.container.message_ts,
        "providerSequence": context.provider_sequence(),
        "userId": interaction.user.id,
        "workspaceId": interaction.team.id,
    }))
    .map_err(|_| connector_error(ConnectorCallErrorKind::Permanent, "SLACK_PAYLOAD_INVALID"))?;
    NormalizedInboundEvent::try_new("slack.interaction.handle", payload)
        .map_err(|_| connector_error(ConnectorCallErrorKind::Permanent, "SLACK_PAYLOAD_INVALID"))
}

fn action_button(
    title: &str,
    action: &str,
    payload: &SlackOutboundPayload,
    style: Option<&str>,
) -> Result<Value, ConnectorCallError> {
    let value = serde_json::to_string(&SlackActionValue {
        action: action.to_owned(),
        interaction_id: payload.interaction_id.clone(),
        expected_revision: payload.expected_revision,
        expires_at_millis: payload.expires_at_millis,
    })
    .map_err(|_| connector_error(ConnectorCallErrorKind::Permanent, "SLACK_PAYLOAD_INVALID"))?;
    let mut button = serde_json::Map::new();
    button.insert("action_id".to_owned(), Value::String(action.to_owned()));
    button.insert(
        "text".to_owned(),
        json!({"text": title, "type": "plain_text"}),
    );
    button.insert("type".to_owned(), Value::String("button".to_owned()));
    button.insert("value".to_owned(), Value::String(value));
    if let Some(style) = style {
        button.insert("style".to_owned(), Value::String(style.to_owned()));
    }
    Ok(Value::Object(button))
}

fn validate_outbound_payload(payload: &SlackOutboundPayload) -> Result<(), ConnectorCallError> {
    if !valid_portable_text(&payload.interaction_id, 256)
        || payload.expected_revision == 0
        || payload.expected_revision > MAX_SAFE_INTEGER
        || payload.expires_at_millis == 0
        || payload.expires_at_millis > MAX_SAFE_INTEGER
        || !valid_block_text(&payload.title, MAX_HEADER_CHARACTERS)
        || !valid_block_text(&payload.body, MAX_SECTION_CHARACTERS)
    {
        return Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "SLACK_PAYLOAD_INVALID",
        ));
    }
    Ok(())
}

fn require_token(
    config: &SlackConnectorConfig,
    token: &SlackBotToken,
) -> Result<(), ConnectorCallError> {
    if token.installation != config.installation
        || token.channel != config.channel
        || !token.permissions.chat_write
        || !token.permissions.history_read
    {
        return Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "SLACK_PERMISSION_DENIED",
        ));
    }
    Ok(())
}

fn require_slack_success(response: &SlackHttpResponse) -> Result<(), ConnectorCallError> {
    if response.status == 429 {
        return Err(rate_limit_error(response.retry_after_seconds));
    }
    if response.status != 200 {
        let kind = match response.status {
            401 => ConnectorCallErrorKind::CredentialRevoked,
            408 | 409 | 425 | 500..=599 => ConnectorCallErrorKind::Retryable,
            _ => ConnectorCallErrorKind::Permanent,
        };
        return Err(connector_error(kind, "SLACK_REQUEST_REJECTED"));
    }
    let body = response.body.as_ref().ok_or_else(response_invalid)?;
    if body.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (kind, code) = match error {
        "invalid_auth" | "token_revoked" | "account_inactive" => (
            ConnectorCallErrorKind::CredentialRevoked,
            "SLACK_CREDENTIAL_REVOKED",
        ),
        "ratelimited" => return Err(rate_limit_error(response.retry_after_seconds)),
        "internal_error" | "fatal_error" | "request_timeout" => (
            ConnectorCallErrorKind::Retryable,
            "SLACK_SERVICE_UNAVAILABLE",
        ),
        _ => (ConnectorCallErrorKind::Permanent, "SLACK_REQUEST_REJECTED"),
    };
    Err(connector_error(kind, code))
}

fn rate_limit_error(retry_after_seconds: Option<u64>) -> ConnectorCallError {
    retry_after_seconds
        .and_then(|seconds| seconds.checked_mul(1_000))
        .and_then(|millis| ConnectorCallError::retryable_after("SLACK_RATE_LIMITED", millis).ok())
        .unwrap_or_else(|| connector_error(ConnectorCallErrorKind::Retryable, "SLACK_RATE_LIMITED"))
}

fn response_invalid() -> ConnectorCallError {
    connector_error(ConnectorCallErrorKind::Retryable, "SLACK_RESPONSE_INVALID")
}

fn operation_key_from_message(message: &Value) -> Option<&str> {
    message
        .get("metadata")?
        .get("event_payload")?
        .get("operation_key")?
        .as_str()
}

fn message_matches_installation(message: &Value, installation: &SlackInstallationIdentity) -> bool {
    let payload = message
        .get("metadata")
        .and_then(|metadata| metadata.get("event_payload"));
    payload.is_some_and(|payload| {
        payload.get("team_id").and_then(Value::as_str) == Some(installation.workspace_id().as_str())
            && payload.get("app_id").and_then(Value::as_str) == Some(installation.app_id().as_str())
            && payload.get("bot_id").and_then(Value::as_str) == Some(installation.bot_id().as_str())
    })
}

fn remote_receipt(
    timestamp: &str,
    remote_write_performed: bool,
) -> Result<OutboundCallReceipt, ConnectorCallError> {
    let mut hash = Sha256::new();
    hash.update(b"winwincode.slack.remote-receipt.v1");
    hash.update([0]);
    hash.update(timestamp.as_bytes());
    OutboundCallReceipt::try_new(
        Sha256Digest(format!("sha256:{:x}", hash.finalize())),
        remote_write_performed,
    )
    .map_err(|_| response_invalid())
}

fn callback_external_id(
    interaction: &SlackInteraction,
    action: &SlackAction,
    value: &SlackActionValue,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"winwincode.slack.callback.v1");
    for part in [
        interaction.team.id.as_str(),
        interaction.channel.id.as_str(),
        interaction.user.id.as_str(),
        interaction.container.message_ts.as_str(),
        action.action_ts.as_str(),
        value.interaction_id.as_str(),
    ] {
        hash.update([0]);
        hash.update(part.as_bytes());
    }
    format!("callback-{:x}", hash.finalize())
}

fn parse_slack_sequence(value: &str) -> Result<u64, IntegrationError> {
    if !valid_slack_timestamp(value) {
        return Err(invalid("Slack action timestamp is invalid"));
    }
    let sequence = value
        .bytes()
        .filter(|byte| *byte != b'.')
        .try_fold(0_u64, |current, byte| {
            current.checked_mul(10)?.checked_add(u64::from(byte - b'0'))
        })
        .ok_or_else(|| invalid("Slack action timestamp is invalid"))?;
    if sequence == 0 || sequence > MAX_SAFE_INTEGER {
        Err(invalid("Slack action timestamp is invalid"))
    } else {
        Ok(sequence)
    }
}

fn valid_slack_timestamp(value: &str) -> bool {
    let Some((seconds, fraction)) = value.split_once('.') else {
        return false;
    };
    matches!(seconds.len(), 1..=10)
        && matches!(fraction.len(), 1..=6)
        && seconds.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.bytes().all(|byte| byte.is_ascii_digit())
}

fn decode_signature(value: &[u8]) -> Option<[u8; 32]> {
    let hex = value.strip_prefix(b"v0=")?;
    if hex.len() != 64 {
        return None;
    }
    let mut output = [0_u8; 32];
    for (index, pair) in hex.chunks_exact(2).enumerate() {
        output[index] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(output)
}

fn signature_credential_error(error: SlackCredentialError) -> SignatureVerificationError {
    match error.kind() {
        SlackCredentialErrorKind::Unavailable => SignatureVerificationError::rejected(),
        SlackCredentialErrorKind::Revoked => SignatureVerificationError::credential_revoked(),
    }
}

fn connector_credential_error(error: SlackCredentialError) -> ConnectorCallError {
    let kind = match error.kind() {
        SlackCredentialErrorKind::Unavailable => ConnectorCallErrorKind::Retryable,
        SlackCredentialErrorKind::Revoked => ConnectorCallErrorKind::CredentialRevoked,
    };
    connector_error(kind, "SLACK_CREDENTIAL_UNAVAILABLE")
}

fn matches_authority(config: &SlackConnectorConfig, authority: &ConnectorAuthority) -> bool {
    authority.integration_id() == &config.integration
        && authority.protocol().as_str() == SLACK_CONNECTOR_PROTOCOL
        && authority.credential_reference_id() == &config.credential_reference
}

fn require_authority(
    config: &SlackConnectorConfig,
    authority: &ConnectorAuthority,
) -> Result<(), ConnectorCallError> {
    if matches_authority(config, authority) {
        Ok(())
    } else {
        Err(connector_error(
            ConnectorCallErrorKind::Permanent,
            "SLACK_AUTHORITY_MISMATCH",
        ))
    }
}

fn connector_error(kind: ConnectorCallErrorKind, code: &'static str) -> ConnectorCallError {
    ConnectorCallError::try_new(kind, code).expect("static Slack connector code must be valid")
}

fn canonical_base_url(value: &str, label: &'static str) -> Result<String, IntegrationError> {
    let uri = ureq::http::Uri::from_str(value).map_err(|_| {
        invalid(if label == "Slack API" {
            "Slack API URL is invalid"
        } else {
            "Control Plane URL is invalid"
        })
    })?;
    if value.len() > 4_096
        || uri.scheme_str() != Some("https")
        || uri.authority().is_none()
        || uri
            .authority()
            .is_some_and(|authority| authority.as_str().contains('@'))
        || uri.query().is_some()
        || value.contains('#')
    {
        return Err(invalid(if label == "Slack API" {
            "Slack API URL is invalid"
        } else {
            "Control Plane URL is invalid"
        }));
    }
    Ok(format!("{}/", value.trim_end_matches('/')))
}

fn validate_tls_roots(value: &SlackTlsRoots) -> Result<(), IntegrationError> {
    if let SlackTlsRoots::Specific(values) = value
        && (values.is_empty() || values.iter().any(Vec::is_empty))
    {
        return Err(invalid("Slack TLS roots are invalid"));
    }
    Ok(())
}

fn validate_prefixed_id(value: &str, prefix: &str) -> Result<(), IntegrationError> {
    let Some(tail) = value.strip_prefix(&format!("{prefix}_")) else {
        return Err(invalid("Slack connector identity is invalid"));
    };
    if tail.len() == 26
        && tail.bytes().all(|byte| {
            matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
        })
    {
        Ok(())
    } else {
        Err(invalid("Slack connector identity is invalid"))
    }
}

fn valid_slack_id(value: &str, prefixes: &[u8]) -> bool {
    matches!(value.len(), 9..=32)
        && value
            .as_bytes()
            .first()
            .is_some_and(|first| prefixes.contains(first))
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn valid_portable_text(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.' | b'@')
        })
}

fn valid_block_text(value: &str, maximum_characters: usize) -> bool {
    !value.is_empty()
        && value.chars().count() <= maximum_characters
        && !value.bytes().any(|byte| byte == 0)
}

fn form_decode(value: &str) -> Result<String, IntegrationError> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let Some(pair) = bytes.get(index + 1..index + 3) else {
                    return Err(invalid("Slack form encoding is invalid"));
                };
                output.push((hex_value(pair[0])? << 4) | hex_value(pair[1])?);
                index += 3;
            }
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(output).map_err(|_| invalid("Slack form encoding is invalid"))
}

fn percent_encode(value: &str) -> String {
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

fn hex_value(value: u8) -> Result<u8, IntegrationError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(invalid("Slack form encoding is invalid")),
    }
}

fn stable_uuid(value: &Sha256Digest) -> String {
    let digest = Sha256::digest(value.0.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut output = String::with_capacity(36);
    for (index, byte) in bytes.into_iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            output.push('-');
        }
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn open_rate_limit_connection(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")?;
    Ok(connection)
}

fn validate_slack_time(value: u64) -> Result<(), ConnectorCallError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        Err(rate_limit_storage_error())
    } else {
        Ok(())
    }
}

fn to_sql_millis(value: u64) -> Result<i64, ConnectorCallError> {
    i64::try_from(value).map_err(|_| rate_limit_storage_error())
}

fn from_sql_millis(value: i64) -> Result<u64, ConnectorCallError> {
    u64::try_from(value)
        .ok()
        .filter(|value| *value > 0 && *value <= MAX_SAFE_INTEGER)
        .ok_or_else(rate_limit_storage_error)
}

fn rate_limit_storage_error() -> ConnectorCallError {
    connector_error(
        ConnectorCallErrorKind::Retryable,
        "SLACK_RATE_LIMIT_STORAGE",
    )
}

const fn slack_storage_error() -> IntegrationError {
    IntegrationError::new(
        IntegrationErrorKind::Storage,
        "Slack rate-limit storage failed",
    )
}

fn invalid(message: &'static str) -> IntegrationError {
    IntegrationError::new(IntegrationErrorKind::Invalid, message)
}
