// SPDX-License-Identifier: Apache-2.0

//! Provider transport values shared by the device runtime and control-plane projections.

use crate::{CredentialLeakGate, ProviderStreamFailureKind, ProviderTokenUsage};
use serde::{Deserialize, Serialize};
use std::fmt;
use winwincode_api::generated::ModelRoute;
use winwincode_domain::{ModelExchangeId, RequestId};

/// Stable Provider Gateway failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderGatewayErrorKind {
    InvalidRequest,
    IdentityDenied,
    IdentityUnavailable,
    RouteUnavailable,
    RouteMismatch,
    ProviderNotFound,
    ProviderDisabled,
    ModelNotFound,
    ModelDisabled,
    StructuredOutputUnsupported,
    CredentialUnavailable,
    CredentialScopeMismatch,
    AdapterNotRegistered,
    AdapterRejected,
    AdapterRateLimited,
    AdapterUnavailable,
    AdapterProtocol,
    ExchangeConflict,
    ExchangeNotFound,
    TerminalConflict,
    AdmissionDenied,
    AdmissionUnavailable,
    SettlementUnavailable,
    CredentialLeak,
    Storage,
}

/// Provider-neutral request exposed to exactly one selected adapter.
///
/// Serialization is intentionally absent and Debug always redacts the body.
pub struct ProviderAdapterInvocation<'a> {
    pub model_exchange_id: &'a ModelExchangeId,
    pub request_id: &'a RequestId,
    pub adapter_request_id: &'a str,
    pub model_id: &'a str,
    pub content_type: &'a str,
    pub payload: &'a [u8],
}

impl ProviderAdapterInvocation<'_> {
    #[must_use]
    pub const fn model_exchange_id(&self) -> &ModelExchangeId {
        self.model_exchange_id
    }

    #[must_use]
    pub const fn request_id(&self) -> &RequestId {
        self.request_id
    }

    /// Returns the precommitted Provider idempotency identity.
    #[must_use]
    pub const fn adapter_request_id(&self) -> &str {
        self.adapter_request_id
    }

    #[must_use]
    pub const fn model_id(&self) -> &str {
        self.model_id
    }

    #[must_use]
    pub const fn content_type(&self) -> &str {
        self.content_type
    }

    /// Borrows the opaque request only for the adapter call.
    #[must_use]
    pub const fn payload(&self) -> &[u8] {
        self.payload
    }
}

impl fmt::Debug for ProviderAdapterInvocation<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAdapterInvocation")
            .field("model_exchange_id", self.model_exchange_id)
            .field("request_id", self.request_id)
            .field("adapter_request_id", &self.adapter_request_id)
            .field("model_id", &self.model_id)
            .field("content_type", &self.content_type)
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

/// Stable Provider adapter failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderAdapterErrorKind {
    Rejected,
    RateLimited,
    Unavailable,
    Protocol,
}

/// Provider adapter error which cannot copy upstream response text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderAdapterError {
    kind: ProviderAdapterErrorKind,
    message: &'static str,
}

impl ProviderAdapterError {
    #[must_use]
    pub const fn rejected() -> Self {
        Self {
            kind: ProviderAdapterErrorKind::Rejected,
            message: "Provider rejected the request",
        }
    }

    #[must_use]
    pub const fn rate_limited() -> Self {
        Self {
            kind: ProviderAdapterErrorKind::RateLimited,
            message: "Provider rate limit rejected the request",
        }
    }

    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            kind: ProviderAdapterErrorKind::Unavailable,
            message: "Provider adapter is unavailable",
        }
    }

    #[must_use]
    pub const fn protocol() -> Self {
        Self {
            kind: ProviderAdapterErrorKind::Protocol,
            message: "Provider adapter response is invalid",
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderAdapterErrorKind {
        self.kind
    }
}

impl fmt::Display for ProviderAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProviderAdapterError {}

/// Secret-free acknowledgement returned after the adapter accepts a request.
#[derive(Clone, Eq, PartialEq)]
pub struct ProviderAdapterOpenReceipt {
    adapter_request_id: String,
}

impl ProviderAdapterOpenReceipt {
    /// Constructs a bounded opaque upstream request identity.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or control-character-containing values.
    pub fn try_new(adapter_request_id: String) -> Result<Self, ProviderAdapterError> {
        if adapter_request_id.is_empty()
            || adapter_request_id.len() > 200
            || adapter_request_id.trim() != adapter_request_id
            || adapter_request_id.chars().any(char::is_control)
        {
            return Err(ProviderAdapterError::protocol());
        }
        Ok(Self { adapter_request_id })
    }

    #[must_use]
    pub fn adapter_request_id(&self) -> &str {
        &self.adapter_request_id
    }
}

impl fmt::Debug for ProviderAdapterOpenReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAdapterOpenReceipt")
            .field("adapter_request_id", &"[REDACTED]")
            .finish()
    }
}

/// One Provider implementation selected by an exact catalog identifier.
pub trait ProviderAdapterPort: Send + Sync {
    /// Exact catalog Provider identifier owned by this adapter.
    fn provider_id(&self) -> &str;

    /// Opens an exchange. The Credential is borrowed only during this call.
    /// `adapter_request_id` is the mandatory upstream idempotency key: exact
    /// retries must recover the first request rather than start another.
    ///
    /// # Errors
    ///
    /// Returns a stable category without exposing Provider response text.
    fn open(
        &self,
        invocation: &ProviderAdapterInvocation<'_>,
        credential: &ResolvedSecret,
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError>;

    /// Applies one transport-level stream control transition. The tuple
    /// (`model_exchange_id`, `adapter_request_id`, `action`) is the mandatory
    /// idempotency identity: exact repeats must return the original result
    /// without applying the Provider effect twice. Cancel and Release for a
    /// precommitted identity whose open side effect has not happened are
    /// successful no-ops that fence any later open using that identity.
    ///
    /// # Errors
    ///
    /// Returns a stable adapter category without Provider response text.
    fn control(
        &self,
        model_exchange_id: &ModelExchangeId,
        adapter_request_id: &str,
        action: ProviderStreamControlAction,
    ) -> Result<(), ProviderAdapterError>;
}

/// Provider transport action owned by the selected adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderStreamControlAction {
    Pause,
    Resume,
    Cancel,
    Release,
}

/// Provider-neutral terminal outcome.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderGatewayTerminalOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

/// Secret-free result of opening an exchange.
pub struct ProviderGatewayOpenReceipt {
    pub model_exchange_id: ModelExchangeId,
    pub request_id: RequestId,
    pub route: ModelRoute,
    pub adapter_request_id: String,
    pub idempotent_replay: bool,
    pub stream_leak_gate: CredentialLeakGate,
}

impl ProviderGatewayOpenReceipt {
    pub fn stream_leak_gate(&self) -> CredentialLeakGate {
        self.stream_leak_gate.fingerprint_snapshot()
    }
}

impl Clone for ProviderGatewayOpenReceipt {
    fn clone(&self) -> Self {
        Self {
            model_exchange_id: self.model_exchange_id.clone(),
            request_id: self.request_id.clone(),
            route: self.route.clone(),
            adapter_request_id: self.adapter_request_id.clone(),
            idempotent_replay: self.idempotent_replay,
            stream_leak_gate: self.stream_leak_gate(),
        }
    }
}

impl fmt::Debug for ProviderGatewayOpenReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderGatewayOpenReceipt")
            .field("model_exchange_id", &self.model_exchange_id)
            .field("request_id", &self.request_id)
            .field("route", &self.route)
            .field("adapter_request_id", &self.adapter_request_id)
            .field("idempotent_replay", &self.idempotent_replay)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ProviderGatewayOpenReceipt {
    fn eq(&self, other: &Self) -> bool {
        self.model_exchange_id == other.model_exchange_id
            && self.request_id == other.request_id
            && self.route == other.route
            && self.adapter_request_id == other.adapter_request_id
            && self.idempotent_replay == other.idempotent_replay
    }
}

impl Eq for ProviderGatewayOpenReceipt {}

/// Trusted terminal command used by the unique stream coordinator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderGatewayTerminalCharge {
    pub usage: ProviderTokenUsage,
    pub actual_cost_micros: u64,
}

/// Trusted terminal command used by the unique stream coordinator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderGatewayTerminal {
    Failed {
        failure: ModelAttemptFailureFact,
        charge: Option<ProviderGatewayTerminalCharge>,
    },
    Cancelled,
    Completed {
        usage: ProviderTokenUsage,
        actual_cost_micros: u64,
    },
}

impl ProviderGatewayTerminal {
    #[must_use]
    pub const fn outcome(self) -> ProviderGatewayTerminalOutcome {
        match self {
            Self::Failed { .. } => ProviderGatewayTerminalOutcome::Failed,
            Self::Cancelled => ProviderGatewayTerminalOutcome::Cancelled,
            Self::Completed { .. } => ProviderGatewayTerminalOutcome::Succeeded,
        }
    }

    pub const fn charge(self) -> Option<ProviderGatewayTerminalCharge> {
        match self {
            Self::Failed { charge, .. } => charge,
            Self::Completed {
                usage,
                actual_cost_micros,
            } => Some(ProviderGatewayTerminalCharge {
                usage,
                actual_cost_micros,
            }),
            Self::Cancelled => None,
        }
    }
}

/// Whether Provider acceptance or output can be ruled out.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelExecutionCertainty {
    /// The request was proven not sent to a Provider.
    NotSent,
    /// The Provider explicitly rejected it before acceptance.
    RejectedBeforeAcceptance,
    /// Acceptance is unknown, so retry may duplicate work or cost.
    AcceptanceUnknown,
    /// At least one output fragment was observed.
    OutputObserved,
}

/// Closed failure class used by retry policy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAttemptFailureKind {
    Authentication,
    InvalidRequest,
    RateLimit,
    Quota,
    Timeout,
    Transport,
    Server,
    ContextWindowExceeded,
    ProviderUnavailable,
    Protocol,
    Cancelled,
    Unknown,
}

/// Secret-free failure fact for one terminal attempt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelAttemptFailureFact {
    /// Stable failure category.
    pub kind: ModelAttemptFailureKind,
    /// Explicit Provider execution certainty.
    pub certainty: ModelExecutionCertainty,
}

impl ModelAttemptFailureFact {
    /// Maps a stable Gateway category without copying Provider diagnostics.
    #[must_use]
    pub const fn from_gateway(
        kind: ProviderGatewayErrorKind,
        certainty: ModelExecutionCertainty,
    ) -> Self {
        let kind = match kind {
            ProviderGatewayErrorKind::AdapterRateLimited => ModelAttemptFailureKind::RateLimit,
            ProviderGatewayErrorKind::AdapterUnavailable
            | ProviderGatewayErrorKind::IdentityUnavailable
            | ProviderGatewayErrorKind::RouteUnavailable
            | ProviderGatewayErrorKind::AdmissionUnavailable
            | ProviderGatewayErrorKind::SettlementUnavailable
            | ProviderGatewayErrorKind::Storage => ModelAttemptFailureKind::ProviderUnavailable,
            ProviderGatewayErrorKind::AdapterProtocol => ModelAttemptFailureKind::Protocol,
            ProviderGatewayErrorKind::AdapterRejected
            | ProviderGatewayErrorKind::InvalidRequest
            | ProviderGatewayErrorKind::IdentityDenied
            | ProviderGatewayErrorKind::RouteMismatch
            | ProviderGatewayErrorKind::ProviderNotFound
            | ProviderGatewayErrorKind::ProviderDisabled
            | ProviderGatewayErrorKind::ModelNotFound
            | ProviderGatewayErrorKind::ModelDisabled
            | ProviderGatewayErrorKind::StructuredOutputUnsupported
            | ProviderGatewayErrorKind::AdapterNotRegistered
            | ProviderGatewayErrorKind::ExchangeConflict
            | ProviderGatewayErrorKind::ExchangeNotFound
            | ProviderGatewayErrorKind::TerminalConflict
            | ProviderGatewayErrorKind::AdmissionDenied
            | ProviderGatewayErrorKind::CredentialLeak => ModelAttemptFailureKind::InvalidRequest,
            ProviderGatewayErrorKind::CredentialUnavailable
            | ProviderGatewayErrorKind::CredentialScopeMismatch => {
                ModelAttemptFailureKind::Authentication
            }
        };
        Self { kind, certainty }
    }

    /// Maps a stable stream failure without copying Provider text or ids.
    #[must_use]
    pub const fn from_stream(
        kind: ProviderStreamFailureKind,
        certainty: ModelExecutionCertainty,
    ) -> Self {
        let kind = match kind {
            ProviderStreamFailureKind::Authentication => ModelAttemptFailureKind::Authentication,
            ProviderStreamFailureKind::InvalidRequest => ModelAttemptFailureKind::InvalidRequest,
            ProviderStreamFailureKind::RateLimit => ModelAttemptFailureKind::RateLimit,
            ProviderStreamFailureKind::Quota => ModelAttemptFailureKind::Quota,
            ProviderStreamFailureKind::Timeout => ModelAttemptFailureKind::Timeout,
            ProviderStreamFailureKind::Transport => ModelAttemptFailureKind::Transport,
            ProviderStreamFailureKind::Server => ModelAttemptFailureKind::Server,
            ProviderStreamFailureKind::ContextWindowExceeded => {
                ModelAttemptFailureKind::ContextWindowExceeded
            }
            ProviderStreamFailureKind::Unknown => ModelAttemptFailureKind::Unknown,
        };
        Self { kind, certainty }
    }

    pub const fn safe_to_retry(self) -> bool {
        matches!(
            self.certainty,
            ModelExecutionCertainty::NotSent | ModelExecutionCertainty::RejectedBeforeAcceptance
        ) && matches!(
            self.kind,
            ModelAttemptFailureKind::RateLimit
                | ModelAttemptFailureKind::Timeout
                | ModelAttemptFailureKind::Transport
                | ModelAttemptFailureKind::Server
                | ModelAttemptFailureKind::ProviderUnavailable
        )
    }
}

/// Exact normalized charge attached to one Provider attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAttemptCharge {
    /// Stable Provider usage identity, unique across every logical request.
    pub provider_usage_id: String,
    /// Provider-normalized token usage.
    pub usage: ProviderTokenUsage,
    /// Actual cost in micros.
    pub cost_micros: u64,
}

/// Opaque secret bytes returned only across the `SecretStore` boundary.
///
/// Debug output is always redacted, serialization is intentionally absent,
/// cloning is intentionally absent, and the owned buffer is cleared on drop.
pub struct ResolvedSecret {
    bytes: Vec<u8>,
}

impl ResolvedSecret {
    /// Takes ownership of non-empty bytes loaded by a `SecretStore` adapter.
    ///
    /// # Errors
    ///
    /// Rejects an empty secret without retaining it in an error.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, SecretStoreError> {
        if bytes.is_empty() {
            return Err(SecretStoreError::corrupt());
        }
        Ok(Self { bytes })
    }

    /// Exposes bytes only at the provider-call boundary.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for ResolvedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ResolvedSecret([REDACTED])")
    }
}

impl Drop for ResolvedSecret {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

/// Stable `SecretStore` failure categories. No adapter diagnostic or remote
/// response is accepted into this public error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretStoreErrorKind {
    Missing,
    VersionConflict,
    Unavailable,
    Corrupt,
}

/// Secret-safe failure returned by a [`SecretStorePort`] implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretStoreError {
    kind: SecretStoreErrorKind,
    message: &'static str,
}

impl SecretStoreError {
    #[must_use]
    pub const fn missing() -> Self {
        Self {
            kind: SecretStoreErrorKind::Missing,
            message: "Credential secret is missing",
        }
    }

    #[must_use]
    pub const fn version_conflict() -> Self {
        Self {
            kind: SecretStoreErrorKind::VersionConflict,
            message: "Credential secret version does not match",
        }
    }

    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            kind: SecretStoreErrorKind::Unavailable,
            message: "Credential secret store is unavailable",
        }
    }

    #[must_use]
    pub const fn corrupt() -> Self {
        Self {
            kind: SecretStoreErrorKind::Corrupt,
            message: "Credential secret record is invalid",
        }
    }

    #[must_use]
    pub const fn kind(&self) -> SecretStoreErrorKind {
        self.kind
    }
}

impl fmt::Display for SecretStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for SecretStoreError {}
