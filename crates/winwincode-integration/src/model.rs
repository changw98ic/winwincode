// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_audit::AuditScope;
use winwincode_domain::{CredentialReferenceId, EnterpriseIntegrationId, Sha256Digest};

use crate::{IntegrationError, IntegrationErrorKind};

pub(crate) const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_PAYLOAD_BYTES: usize = 1_048_576;
const MAX_TEXT_BYTES: usize = 128;

/// Stable protocol mapper name without a concrete provider dependency.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ConnectorProtocol(String);

impl ConnectorProtocol {
    /// Builds a protocol name such as `webhook.v1`.
    ///
    /// # Errors
    ///
    /// Rejects empty, overlong, or non-portable names.
    pub fn try_new(value: impl Into<String>) -> Result<Self, IntegrationError> {
        let value = value.into();
        validate_name(&value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Digest identity used for one outbound idempotency operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct IntegrationOperationKey(Sha256Digest);

impl IntegrationOperationKey {
    /// Derives a non-reversible operation key from one caller-owned identity.
    ///
    /// # Errors
    ///
    /// Rejects empty or overlong input.
    pub fn derive(value: &str) -> Result<Self, IntegrationError> {
        if value.is_empty() || value.len() > 512 {
            return Err(invalid());
        }
        Ok(Self(domain_digest(
            b"winwincode.integration.operation.v1",
            value.as_bytes(),
        )))
    }

    pub(crate) fn from_stored(value: Sha256Digest) -> Result<Self, IntegrationError> {
        validate_digest(&value).map_err(|_| corrupt())?;
        Ok(Self(value))
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.0
    }
}

/// Canonical short-lived outbound claim identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct IntegrationLeaseId(String);

impl IntegrationLeaseId {
    /// Builds a canonical integration lease identity.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical identifier.
    pub fn try_new(value: impl Into<String>) -> Result<Self, IntegrationError> {
        let value = value.into();
        validate_id(&value, "igl")?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Durable credential-call state for one connector.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorState {
    Active,
    CredentialRevoked,
}

/// Initial connector authority facts.
#[derive(Clone, Debug)]
pub struct ConnectorRegistration {
    integration_id: EnterpriseIntegrationId,
    scope: AuditScope,
    protocol: ConnectorProtocol,
    credential_reference_id: CredentialReferenceId,
    registered_at_millis: u64,
}

impl ConnectorRegistration {
    /// Builds one tenant-scoped connector registration.
    ///
    /// # Errors
    ///
    /// Rejects invalid tenant, credential reference, or time facts.
    pub fn try_new(
        integration_id: EnterpriseIntegrationId,
        scope: AuditScope,
        protocol: ConnectorProtocol,
        credential_reference_id: CredentialReferenceId,
        registered_at_millis: u64,
    ) -> Result<Self, IntegrationError> {
        validate_integration_id(&integration_id)?;
        validate_scope(&scope)?;
        validate_id(&credential_reference_id.0, "crd")?;
        validate_time(registered_at_millis)?;
        Ok(Self {
            integration_id,
            scope,
            protocol,
            credential_reference_id,
            registered_at_millis,
        })
    }

    #[must_use]
    pub const fn integration_id(&self) -> &EnterpriseIntegrationId {
        &self.integration_id
    }
    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }
    #[must_use]
    pub const fn protocol(&self) -> &ConnectorProtocol {
        &self.protocol
    }
    #[must_use]
    pub const fn credential_reference_id(&self) -> &CredentialReferenceId {
        &self.credential_reference_id
    }
    #[must_use]
    pub const fn registered_at_millis(&self) -> u64 {
        self.registered_at_millis
    }
}

/// Exact active or revoked connector authority loaded from durable storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectorAuthority {
    integration_id: EnterpriseIntegrationId,
    scope: AuditScope,
    protocol: ConnectorProtocol,
    credential_reference_id: CredentialReferenceId,
    revision: u64,
    state: ConnectorState,
    updated_at_millis: u64,
}

impl ConnectorAuthority {
    pub(crate) fn from_stored(
        integration_id: EnterpriseIntegrationId,
        scope: AuditScope,
        protocol: ConnectorProtocol,
        credential_reference_id: CredentialReferenceId,
        revision: u64,
        state: ConnectorState,
        updated_at_millis: u64,
    ) -> Result<Self, IntegrationError> {
        validate_integration_id(&integration_id).map_err(|_| corrupt())?;
        validate_scope(&scope)?;
        validate_id(&credential_reference_id.0, "crd")?;
        validate_count(revision)?;
        if revision == 0 {
            return Err(corrupt());
        }
        validate_time(updated_at_millis).map_err(|_| corrupt())?;
        Ok(Self {
            integration_id,
            scope,
            protocol,
            credential_reference_id,
            revision,
            state,
            updated_at_millis,
        })
    }

    #[must_use]
    pub const fn integration_id(&self) -> &EnterpriseIntegrationId {
        &self.integration_id
    }
    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }
    #[must_use]
    pub const fn protocol(&self) -> &ConnectorProtocol {
        &self.protocol
    }
    #[must_use]
    pub const fn credential_reference_id(&self) -> &CredentialReferenceId {
        &self.credential_reference_id
    }
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }
    #[must_use]
    pub const fn state(&self) -> ConnectorState {
        self.state
    }
    #[must_use]
    pub const fn updated_at_millis(&self) -> u64 {
        self.updated_at_millis
    }
}

/// Durable registration response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectorRegistrationReceipt {
    authority: ConnectorAuthority,
    idempotent_replay: bool,
}

impl ConnectorRegistrationReceipt {
    pub(crate) const fn new(authority: ConnectorAuthority, idempotent_replay: bool) -> Self {
        Self {
            authority,
            idempotent_replay,
        }
    }

    #[must_use]
    pub const fn authority(&self) -> &ConnectorAuthority {
        &self.authority
    }
    #[must_use]
    pub const fn idempotent_replay(&self) -> bool {
        self.idempotent_replay
    }
}

/// Raw inbound request that is never written to durable storage.
#[derive(Clone, Debug)]
pub struct InboundWebhookMetadata {
    event_type: String,
    external_event_id: String,
    ordering_key: String,
    provider_sequence: u64,
    received_at_millis: u64,
}

impl InboundWebhookMetadata {
    /// Builds bounded provider metadata used for exact replay and stream-local ordering.
    ///
    /// # Errors
    ///
    /// Rejects invalid event names, identifiers, sequence, or time.
    pub fn try_new(
        event_type: impl Into<String>,
        external_event_id: impl Into<String>,
        ordering_key: impl Into<String>,
        provider_sequence: u64,
        received_at_millis: u64,
    ) -> Result<Self, IntegrationError> {
        let event_type = event_type.into();
        let external_event_id = external_event_id.into();
        let ordering_key = ordering_key.into();
        validate_name(&event_type)?;
        validate_time(received_at_millis)?;
        if external_event_id.is_empty()
            || external_event_id.len() > 512
            || ordering_key.is_empty()
            || ordering_key.len() > 512
            || provider_sequence == 0
            || provider_sequence > MAX_SAFE_INTEGER
        {
            return Err(invalid());
        }
        Ok(Self {
            event_type,
            external_event_id,
            ordering_key,
            provider_sequence,
            received_at_millis,
        })
    }

    #[must_use]
    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    #[must_use]
    pub fn external_event_id(&self) -> &str {
        &self.external_event_id
    }

    #[must_use]
    pub fn ordering_key(&self) -> &str {
        &self.ordering_key
    }

    #[must_use]
    pub const fn provider_sequence(&self) -> u64 {
        self.provider_sequence
    }

    #[must_use]
    pub const fn received_at_millis(&self) -> u64 {
        self.received_at_millis
    }
}

/// Raw inbound request that is never written to durable storage.
#[derive(Clone, Debug)]
pub struct InboundWebhookRequest {
    integration_id: EnterpriseIntegrationId,
    scope: AuditScope,
    metadata: InboundWebhookMetadata,
    signature: Vec<u8>,
    payload: Vec<u8>,
}

impl InboundWebhookRequest {
    /// Builds one bounded inbound request.
    ///
    /// # Errors
    ///
    /// Rejects invalid scope, sequence, time, or oversized raw inputs.
    pub fn try_new(
        integration_id: EnterpriseIntegrationId,
        scope: AuditScope,
        metadata: InboundWebhookMetadata,
        signature: Vec<u8>,
        payload: Vec<u8>,
    ) -> Result<Self, IntegrationError> {
        validate_integration_id(&integration_id)?;
        validate_scope(&scope)?;
        if signature.is_empty()
            || signature.len() > 16_384
            || payload.is_empty()
            || payload.len() > MAX_PAYLOAD_BYTES
        {
            return Err(invalid());
        }
        Ok(Self {
            integration_id,
            scope,
            metadata,
            signature,
            payload,
        })
    }

    #[must_use]
    pub const fn integration_id(&self) -> &EnterpriseIntegrationId {
        &self.integration_id
    }
    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }
    #[must_use]
    pub fn external_event_id(&self) -> &str {
        self.metadata.external_event_id()
    }
    #[must_use]
    pub fn event_type(&self) -> &str {
        self.metadata.event_type()
    }
    #[must_use]
    pub fn ordering_key(&self) -> &str {
        self.metadata.ordering_key()
    }
    #[must_use]
    pub const fn provider_sequence(&self) -> u64 {
        self.metadata.provider_sequence()
    }
    #[must_use]
    pub fn signature(&self) -> &[u8] {
        &self.signature
    }
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
    #[must_use]
    pub const fn received_at_millis(&self) -> u64 {
        self.metadata.received_at_millis()
    }
    #[must_use]
    pub(crate) fn event_key(&self) -> Sha256Digest {
        domain_digest(
            b"winwincode.integration.inbound-event.v1",
            self.metadata.external_event_id().as_bytes(),
        )
    }
    #[must_use]
    pub(crate) fn ordering_key_digest(&self) -> Sha256Digest {
        domain_digest(
            b"winwincode.integration.inbound-ordering-key.v1",
            self.metadata.ordering_key().as_bytes(),
        )
    }
    #[must_use]
    pub(crate) fn payload_digest(&self) -> Sha256Digest {
        domain_digest(b"winwincode.integration.raw-payload.v1", &self.payload)
    }
}

/// Secret-free context supplied to a provider protocol normalizer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboundNormalizationContext {
    event_type: String,
    event_key: Sha256Digest,
    ordering_key_digest: Sha256Digest,
    provider_sequence: u64,
    received_at_millis: u64,
}

impl InboundNormalizationContext {
    pub(crate) fn from_request(request: &InboundWebhookRequest) -> Self {
        Self {
            event_type: request.event_type().to_owned(),
            event_key: request.event_key(),
            ordering_key_digest: request.ordering_key_digest(),
            provider_sequence: request.provider_sequence(),
            received_at_millis: request.received_at_millis(),
        }
    }

    #[must_use]
    pub fn event_type(&self) -> &str {
        &self.event_type
    }
    #[must_use]
    pub const fn event_key(&self) -> &Sha256Digest {
        &self.event_key
    }
    #[must_use]
    pub const fn ordering_key_digest(&self) -> &Sha256Digest {
        &self.ordering_key_digest
    }
    #[must_use]
    pub const fn provider_sequence(&self) -> u64 {
        self.provider_sequence
    }
    #[must_use]
    pub const fn received_at_millis(&self) -> u64 {
        self.received_at_millis
    }
}

/// Canonical protocol output forwarded later to a formal Control Plane command adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedInboundEvent {
    name: String,
    payload: Vec<u8>,
    digest: Sha256Digest,
}

impl NormalizedInboundEvent {
    /// Builds one canonical JSON command payload.
    ///
    /// # Errors
    ///
    /// Rejects invalid names, non-canonical JSON, or oversized payloads.
    pub fn try_new(
        command_name: impl Into<String>,
        command_payload: Vec<u8>,
    ) -> Result<Self, IntegrationError> {
        let command_name = command_name.into();
        validate_name(&command_name)?;
        validate_canonical_json(&command_payload)?;
        let digest = domain_digest(
            b"winwincode.integration.normalized-command.v1",
            &command_payload,
        );
        Ok(Self {
            name: command_name,
            payload: command_payload,
            digest,
        })
    }

    #[must_use]
    pub fn command_name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub fn command_payload(&self) -> &[u8] {
        &self.payload
    }
    #[must_use]
    pub const fn command_digest(&self) -> &Sha256Digest {
        &self.digest
    }
}

/// Durable disposition for one authenticated inbound event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundStatus {
    Accepted,
    IgnoredOutOfOrder,
}

/// Immutable inbound delivery receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboundReceipt {
    integration_id: EnterpriseIntegrationId,
    event_key: Sha256Digest,
    ordering_key_digest: Sha256Digest,
    payload_digest: Sha256Digest,
    provider_sequence: u64,
    status: InboundStatus,
    command_digest: Sha256Digest,
    received_at_millis: u64,
    idempotent_replay: bool,
}

pub(crate) struct StoredInboundReceipt {
    pub integration_id: EnterpriseIntegrationId,
    pub event_key: Sha256Digest,
    pub ordering_key_digest: Sha256Digest,
    pub payload_digest: Sha256Digest,
    pub provider_sequence: u64,
    pub status: InboundStatus,
    pub command_digest: Sha256Digest,
    pub received_at_millis: u64,
}

impl InboundReceipt {
    pub(crate) fn from_stored(stored: StoredInboundReceipt) -> Self {
        Self {
            integration_id: stored.integration_id,
            event_key: stored.event_key,
            ordering_key_digest: stored.ordering_key_digest,
            payload_digest: stored.payload_digest,
            provider_sequence: stored.provider_sequence,
            status: stored.status,
            command_digest: stored.command_digest,
            received_at_millis: stored.received_at_millis,
            idempotent_replay: false,
        }
    }

    pub(crate) const fn with_replay(mut self, idempotent_replay: bool) -> Self {
        self.idempotent_replay = idempotent_replay;
        self
    }

    #[must_use]
    pub const fn integration_id(&self) -> &EnterpriseIntegrationId {
        &self.integration_id
    }
    #[must_use]
    pub const fn event_key(&self) -> &Sha256Digest {
        &self.event_key
    }
    #[must_use]
    pub const fn status(&self) -> InboundStatus {
        self.status
    }
    #[must_use]
    pub const fn provider_sequence(&self) -> u64 {
        self.provider_sequence
    }
    #[must_use]
    pub const fn ordering_key_digest(&self) -> &Sha256Digest {
        &self.ordering_key_digest
    }
    #[must_use]
    pub const fn idempotent_replay(&self) -> bool {
        self.idempotent_replay
    }
    #[must_use]
    pub const fn payload_digest(&self) -> &Sha256Digest {
        &self.payload_digest
    }
    #[must_use]
    pub const fn command_digest(&self) -> &Sha256Digest {
        &self.command_digest
    }
    #[must_use]
    pub const fn received_at_millis(&self) -> u64 {
        self.received_at_millis
    }
}

/// One durable normalized command awaiting the formal Control Plane adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboundDispatch {
    sequence: u64,
    integration_id: EnterpriseIntegrationId,
    event_key: Sha256Digest,
    command_name: String,
    command_payload: Vec<u8>,
    command_digest: Sha256Digest,
}

impl InboundDispatch {
    pub(crate) const fn from_stored(
        sequence: u64,
        integration_id: EnterpriseIntegrationId,
        event_key: Sha256Digest,
        command_name: String,
        command_payload: Vec<u8>,
        command_digest: Sha256Digest,
    ) -> Self {
        Self {
            sequence,
            integration_id,
            event_key,
            command_name,
            command_payload,
            command_digest,
        }
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    #[must_use]
    pub const fn integration_id(&self) -> &EnterpriseIntegrationId {
        &self.integration_id
    }
    #[must_use]
    pub const fn event_key(&self) -> &Sha256Digest {
        &self.event_key
    }
    #[must_use]
    pub fn command_name(&self) -> &str {
        &self.command_name
    }
    #[must_use]
    pub fn command_payload(&self) -> &[u8] {
        &self.command_payload
    }
    #[must_use]
    pub const fn command_digest(&self) -> &Sha256Digest {
        &self.command_digest
    }
}

/// Finite capped exponential retry policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    max_attempts: u32,
    initial_backoff_millis: u64,
    max_backoff_millis: u64,
}

impl RetryPolicy {
    /// Builds a bounded retry policy.
    ///
    /// # Errors
    ///
    /// Rejects zero, reversed, or unsafe limits.
    pub fn try_new(
        max_attempts: u32,
        initial_backoff_millis: u64,
        max_backoff_millis: u64,
    ) -> Result<Self, IntegrationError> {
        if max_attempts == 0
            || initial_backoff_millis == 0
            || max_backoff_millis < initial_backoff_millis
            || max_backoff_millis > MAX_SAFE_INTEGER
        {
            return Err(invalid());
        }
        Ok(Self {
            max_attempts,
            initial_backoff_millis,
            max_backoff_millis,
        })
    }

    #[must_use]
    pub const fn max_attempts(self) -> u32 {
        self.max_attempts
    }
    #[must_use]
    pub const fn initial_backoff_millis(self) -> u64 {
        self.initial_backoff_millis
    }
    #[must_use]
    pub const fn max_backoff_millis(self) -> u64 {
        self.max_backoff_millis
    }

    pub(crate) fn retry_at(self, attempt: u32, failed_at: u64) -> Result<u64, IntegrationError> {
        if attempt == 0 || attempt > self.max_attempts {
            return Err(corrupt());
        }
        let shift = attempt.saturating_sub(1).min(62);
        let factor = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
        let delay = self
            .initial_backoff_millis
            .saturating_mul(factor)
            .min(self.max_backoff_millis);
        let retry_at = failed_at.checked_add(delay).ok_or_else(invalid)?;
        validate_time(retry_at)?;
        Ok(retry_at)
    }
}

/// One durable outbound request. The payload must be canonical JSON.
#[derive(Clone, Debug)]
pub struct OutboundRequest {
    integration_id: EnterpriseIntegrationId,
    scope: AuditScope,
    operation_key: IntegrationOperationKey,
    operation_name: String,
    payload: Vec<u8>,
    request_digest: Sha256Digest,
    retry_policy: RetryPolicy,
    enqueued_at_millis: u64,
}

impl OutboundRequest {
    /// Builds one retry-stable outbound request.
    ///
    /// # Errors
    ///
    /// Rejects invalid tenant, name, payload, or time facts.
    pub fn try_new(
        integration_id: EnterpriseIntegrationId,
        scope: AuditScope,
        operation_key: IntegrationOperationKey,
        operation_name: impl Into<String>,
        payload: Vec<u8>,
        retry_policy: RetryPolicy,
        enqueued_at_millis: u64,
    ) -> Result<Self, IntegrationError> {
        let operation_name = operation_name.into();
        validate_integration_id(&integration_id)?;
        validate_scope(&scope)?;
        validate_name(&operation_name)?;
        validate_canonical_json(&payload)?;
        validate_time(enqueued_at_millis)?;
        let request_digest = outbound_request_digest(&operation_name, &payload, retry_policy);
        Ok(Self {
            integration_id,
            scope,
            operation_key,
            operation_name,
            payload,
            request_digest,
            retry_policy,
            enqueued_at_millis,
        })
    }

    #[must_use]
    pub const fn integration_id(&self) -> &EnterpriseIntegrationId {
        &self.integration_id
    }
    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }
    #[must_use]
    pub const fn operation_key(&self) -> &IntegrationOperationKey {
        &self.operation_key
    }
    #[must_use]
    pub fn operation_name(&self) -> &str {
        &self.operation_name
    }
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
    #[must_use]
    pub const fn request_digest(&self) -> &Sha256Digest {
        &self.request_digest
    }
    #[must_use]
    pub const fn retry_policy(&self) -> RetryPolicy {
        self.retry_policy
    }
    #[must_use]
    pub const fn enqueued_at_millis(&self) -> u64 {
        self.enqueued_at_millis
    }
}

/// Durable outbound queue state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundOperationState {
    Pending,
    Leased,
    Delivered,
    DeadLetter,
}

/// Current secret-free outbound operation projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboundOperation {
    integration_id: EnterpriseIntegrationId,
    operation_key: IntegrationOperationKey,
    request_digest: Sha256Digest,
    state: OutboundOperationState,
    attempt: u32,
    eligible_at_millis: u64,
}

impl OutboundOperation {
    pub(crate) const fn from_stored(
        integration_id: EnterpriseIntegrationId,
        operation_key: IntegrationOperationKey,
        request_digest: Sha256Digest,
        state: OutboundOperationState,
        attempt: u32,
        eligible_at_millis: u64,
    ) -> Self {
        Self {
            integration_id,
            operation_key,
            request_digest,
            state,
            attempt,
            eligible_at_millis,
        }
    }

    #[must_use]
    pub const fn integration_id(&self) -> &EnterpriseIntegrationId {
        &self.integration_id
    }
    #[must_use]
    pub const fn operation_key(&self) -> &IntegrationOperationKey {
        &self.operation_key
    }
    #[must_use]
    pub const fn state(&self) -> OutboundOperationState {
        self.state
    }
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }
    #[must_use]
    pub const fn eligible_at_millis(&self) -> u64 {
        self.eligible_at_millis
    }
    pub(crate) const fn request_digest(&self) -> &Sha256Digest {
        &self.request_digest
    }
}

/// Idempotent enqueue response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboundEnqueueReceipt {
    operation: OutboundOperation,
    idempotent_replay: bool,
}

impl OutboundEnqueueReceipt {
    pub(crate) const fn new(operation: OutboundOperation, idempotent_replay: bool) -> Self {
        Self {
            operation,
            idempotent_replay,
        }
    }
    #[must_use]
    pub const fn operation(&self) -> &OutboundOperation {
        &self.operation
    }
    #[must_use]
    pub const fn idempotent_replay(&self) -> bool {
        self.idempotent_replay
    }
}

/// Exact leased call handed to a protocol adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboundClaim {
    authority: ConnectorAuthority,
    operation_key: IntegrationOperationKey,
    request_digest: Sha256Digest,
    operation_name: String,
    payload: Vec<u8>,
    attempt: u32,
    lease_id: IntegrationLeaseId,
}

impl OutboundClaim {
    pub(crate) const fn from_stored(
        authority: ConnectorAuthority,
        operation_key: IntegrationOperationKey,
        request_digest: Sha256Digest,
        operation_name: String,
        payload: Vec<u8>,
        attempt: u32,
        lease_id: IntegrationLeaseId,
    ) -> Self {
        Self {
            authority,
            operation_key,
            request_digest,
            operation_name,
            payload,
            attempt,
            lease_id,
        }
    }

    #[must_use]
    pub const fn authority(&self) -> &ConnectorAuthority {
        &self.authority
    }
    #[must_use]
    pub const fn operation_key(&self) -> &IntegrationOperationKey {
        &self.operation_key
    }
    #[must_use]
    pub const fn request_digest(&self) -> &Sha256Digest {
        &self.request_digest
    }
    #[must_use]
    pub fn operation_name(&self) -> &str {
        &self.operation_name
    }
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }
    #[must_use]
    pub const fn lease_id(&self) -> &IntegrationLeaseId {
        &self.lease_id
    }
}

/// Successful remote call receipt returned by a concrete protocol adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboundCallReceipt {
    remote_receipt_digest: Sha256Digest,
    remote_write_performed: bool,
}

impl OutboundCallReceipt {
    /// Builds a secret-free remote receipt.
    ///
    /// # Errors
    ///
    /// Rejects a malformed digest.
    pub fn try_new(
        remote_receipt_digest: Sha256Digest,
        remote_write_performed: bool,
    ) -> Result<Self, IntegrationError> {
        validate_digest(&remote_receipt_digest)?;
        Ok(Self {
            remote_receipt_digest,
            remote_write_performed,
        })
    }

    #[must_use]
    pub const fn remote_receipt_digest(&self) -> &Sha256Digest {
        &self.remote_receipt_digest
    }
    #[must_use]
    pub const fn remote_write_performed(&self) -> bool {
        self.remote_write_performed
    }
}

/// Immutable terminal outbound delivery/dead-letter receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboundDeliveryReceipt {
    operation: OutboundOperation,
    remote_receipt_digest: Option<Sha256Digest>,
    remote_write_performed: Option<bool>,
    completed_at_millis: u64,
    idempotent_replay: bool,
}

impl OutboundDeliveryReceipt {
    pub(crate) const fn from_stored(
        operation: OutboundOperation,
        remote_receipt_digest: Option<Sha256Digest>,
        remote_write_performed: Option<bool>,
        completed_at_millis: u64,
        idempotent_replay: bool,
    ) -> Self {
        Self {
            operation,
            remote_receipt_digest,
            remote_write_performed,
            completed_at_millis,
            idempotent_replay,
        }
    }
    #[must_use]
    pub const fn operation(&self) -> &OutboundOperation {
        &self.operation
    }
    #[must_use]
    pub const fn remote_receipt_digest(&self) -> Option<&Sha256Digest> {
        self.remote_receipt_digest.as_ref()
    }
    #[must_use]
    pub const fn remote_write_performed(&self) -> Option<bool> {
        self.remote_write_performed
    }
    #[must_use]
    pub const fn completed_at_millis(&self) -> u64 {
        self.completed_at_millis
    }
    #[must_use]
    pub const fn idempotent_replay(&self) -> bool {
        self.idempotent_replay
    }
}

/// Result of one attempted outbound delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutboundAttemptResult {
    Delivered(OutboundDeliveryReceipt),
    RetryScheduled(OutboundOperation),
    DeadLettered(OutboundDeliveryReceipt),
}

/// Secret-free integration audit categories persisted in the audit outbox.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationAuditKind {
    ConnectorRegistered,
    CredentialRevoked,
    InboundAccepted,
    InboundIgnored,
    OutboundEnqueued,
    OutboundDelivered,
    OutboundRetryScheduled,
    OutboundDeadLettered,
}

/// Secret-safe durable audit fact. It contains only tenant identity, connector
/// identity, non-reversible request identity, stable outcome, and time.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationAuditFact {
    sequence: u64,
    scope: AuditScope,
    integration_id: EnterpriseIntegrationId,
    kind: IntegrationAuditKind,
    request_digest: Sha256Digest,
    occurred_at_millis: u64,
}

impl IntegrationAuditFact {
    pub(crate) const fn from_stored(
        sequence: u64,
        scope: AuditScope,
        integration_id: EnterpriseIntegrationId,
        kind: IntegrationAuditKind,
        request_digest: Sha256Digest,
        occurred_at_millis: u64,
    ) -> Self {
        Self {
            sequence,
            scope,
            integration_id,
            kind,
            request_digest,
            occurred_at_millis,
        }
    }
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }
    #[must_use]
    pub const fn integration_id(&self) -> &EnterpriseIntegrationId {
        &self.integration_id
    }
    #[must_use]
    pub const fn kind(&self) -> IntegrationAuditKind {
        self.kind
    }
    #[must_use]
    pub const fn request_digest(&self) -> &Sha256Digest {
        &self.request_digest
    }
    #[must_use]
    pub const fn occurred_at_millis(&self) -> u64 {
        self.occurred_at_millis
    }
}

pub(crate) fn validate_scope(scope: &AuditScope) -> Result<(), IntegrationError> {
    let valid = match scope {
        AuditScope::Organization { organization_id } => canonical_id(&organization_id.0, "org"),
        AuditScope::Workspace {
            organization_id,
            workspace_id,
        } => canonical_id(&organization_id.0, "org") && canonical_id(&workspace_id.0, "wsp"),
        AuditScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => {
            canonical_id(&organization_id.0, "org")
                && canonical_id(&workspace_id.0, "wsp")
                && canonical_id(&project_id.0, "prj")
        }
        AuditScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => {
            canonical_id(&organization_id.0, "org")
                && canonical_id(&workspace_id.0, "wsp")
                && canonical_id(&project_id.0, "prj")
                && canonical_id(&repository_id.0, "rep")
        }
    };
    if valid { Ok(()) } else { Err(invalid()) }
}

pub(crate) fn validate_integration_id(
    integration_id: &EnterpriseIntegrationId,
) -> Result<(), IntegrationError> {
    validate_id(&integration_id.0, "int")
}

pub(crate) fn scope_bytes(scope: &AuditScope) -> Result<Vec<u8>, IntegrationError> {
    validate_scope(scope)?;
    serde_json::to_vec(scope).map_err(|_| corrupt())
}

pub(crate) fn validate_time(value: u64) -> Result<(), IntegrationError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        Err(invalid())
    } else {
        Ok(())
    }
}

pub(crate) fn validate_count(value: u64) -> Result<(), IntegrationError> {
    if value > MAX_SAFE_INTEGER {
        Err(corrupt())
    } else {
        Ok(())
    }
}

pub(crate) fn validate_digest(value: &Sha256Digest) -> Result<(), IntegrationError> {
    let valid = value.0.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    });
    if valid { Ok(()) } else { Err(invalid()) }
}

fn validate_canonical_json(bytes: &[u8]) -> Result<(), IntegrationError> {
    if bytes.is_empty() || bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(invalid());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if serde_json::to_vec(&value).map_err(|_| invalid())? != bytes {
        return Err(invalid());
    }
    Ok(())
}

fn validate_name(value: &str) -> Result<(), IntegrationError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_TEXT_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        });
    if valid { Ok(()) } else { Err(invalid()) }
}

fn validate_id(value: &str, prefix: &str) -> Result<(), IntegrationError> {
    if canonical_id(value, prefix) {
        Ok(())
    } else {
        Err(invalid())
    }
}

fn canonical_id(value: &str, prefix: &str) -> bool {
    const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    value
        .strip_prefix(&format!("{prefix}_"))
        .is_some_and(|suffix| {
            suffix.len() == 26 && suffix.bytes().all(|byte| CROCKFORD.contains(&byte))
        })
}

pub(crate) fn domain_digest(domain: &[u8], bytes: &[u8]) -> Sha256Digest {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update([0]);
    hash.update(bytes);
    Sha256Digest(format!("sha256:{:x}", hash.finalize()))
}

fn outbound_request_digest(
    operation_name: &str,
    payload: &[u8],
    retry_policy: RetryPolicy,
) -> Sha256Digest {
    let mut bytes = Vec::with_capacity(operation_name.len() + payload.len() + 32);
    bytes.extend_from_slice(operation_name.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&retry_policy.max_attempts.to_be_bytes());
    bytes.extend_from_slice(&retry_policy.initial_backoff_millis.to_be_bytes());
    bytes.extend_from_slice(&retry_policy.max_backoff_millis.to_be_bytes());
    domain_digest(b"winwincode.integration.outbound-request.v1", &bytes)
}

pub(crate) const fn invalid() -> IntegrationError {
    IntegrationError::new(
        IntegrationErrorKind::Invalid,
        "integration facts are invalid",
    )
}

pub(crate) const fn corrupt() -> IntegrationError {
    IntegrationError::new(
        IntegrationErrorKind::CorruptState,
        "integration durable state is corrupt",
    )
}
