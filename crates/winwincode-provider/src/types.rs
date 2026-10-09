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
    AdapterRequestInvalid,
    AdapterRequestTranslation,
    AdapterRequestSizeLimit,
    AdapterResponseContentType,
    AdapterConnection,
    AdapterUpstream,
    AdapterIdentityConflict,
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
    RequestInvalid,
    RequestTranslation,
    RequestSizeLimit,
    ResponseContentType,
    Connection,
    Upstream,
    IdentityConflict,
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
    http_status: Option<u16>,
    retry_after: Option<std::time::Duration>,
    connection_pending: bool,
    network: Option<Box<winwincode_network::NetworkFailure>>,
}

impl ProviderAdapterError {
    const fn new(kind: ProviderAdapterErrorKind, message: &'static str) -> Self {
        Self {
            kind,
            message,
            http_status: None,
            retry_after: None,
            connection_pending: false,
            network: None,
        }
    }

    pub(crate) fn from_network(failure: winwincode_network::NetworkFailure) -> Self {
        use winwincode_network::ErrorKind;
        let mut error = match failure.kind {
            ErrorKind::ConnectionUnavailable
            | ErrorKind::TransportInterrupted
            | ErrorKind::Timeout => Self::connection(),
            ErrorKind::RateLimited => Self::rate_limited(),
            ErrorKind::ServerTransient => Self::upstream(),
            ErrorKind::Authentication | ErrorKind::Authorization | ErrorKind::Cancelled => {
                Self::rejected()
            }
            ErrorKind::RequestInvalid => Self::request_invalid(),
            _ => Self::protocol(),
        };
        error.connection_pending =
            failure.acceptance == winwincode_network::Acceptance::NotSent && failure.retryable();
        error.http_status = failure.http_status;
        error.retry_after = failure.retry_after_ms.map(std::time::Duration::from_millis);
        error.network = Some(Box::new(failure));
        error
    }

    /// Bounded request facts without upstream text or credentials.
    #[must_use]
    pub fn network_failure(&self) -> winwincode_network::NetworkFailure {
        use winwincode_network::{Acceptance, ErrorKind, NetworkFailure, Phase};
        self.network.as_deref().copied().unwrap_or_else(|| {
            if let Some(status) = self.http_status {
                return NetworkFailure::http(status, self.retry_after);
            }
            let kind = match self.kind {
                ProviderAdapterErrorKind::Connection => ErrorKind::TransportInterrupted,
                ProviderAdapterErrorKind::Upstream => ErrorKind::ServerTransient,
                ProviderAdapterErrorKind::RateLimited => ErrorKind::RateLimited,
                ProviderAdapterErrorKind::RequestInvalid
                | ProviderAdapterErrorKind::RequestTranslation
                | ProviderAdapterErrorKind::RequestSizeLimit
                | ProviderAdapterErrorKind::Rejected => ErrorKind::RequestInvalid,
                ProviderAdapterErrorKind::IdentityConflict => ErrorKind::IntegrityInvalid,
                ProviderAdapterErrorKind::Unavailable => ErrorKind::StorageUnavailable,
                ProviderAdapterErrorKind::ResponseContentType
                | ProviderAdapterErrorKind::Protocol => ErrorKind::ProtocolInvalid,
            };
            let mut failure = NetworkFailure::new(
                if self.connection_pending {
                    ErrorKind::ConnectionUnavailable
                } else {
                    kind
                },
                if self.connection_pending {
                    Acceptance::NotSent
                } else {
                    Acceptance::Unknown
                },
                Phase::ResponseHeaders,
            );
            failure.retry_after_ms = self.retry_after.map(winwincode_network::duration_millis);
            failure.diagnostic = Some(winwincode_network::NetworkDiagnostic::new(
                match self.kind {
                    ProviderAdapterErrorKind::ResponseContentType => {
                        winwincode_network::DiagnosticCode::ContentType
                    }
                    ProviderAdapterErrorKind::Protocol => {
                        winwincode_network::DiagnosticCode::HttpProtocol
                    }
                    ProviderAdapterErrorKind::RequestSizeLimit => {
                        winwincode_network::DiagnosticCode::BodyTooLarge
                    }
                    ProviderAdapterErrorKind::IdentityConflict => {
                        winwincode_network::DiagnosticCode::IdentityConflict
                    }
                    ProviderAdapterErrorKind::Unavailable => {
                        winwincode_network::DiagnosticCode::Storage
                    }
                    ProviderAdapterErrorKind::Connection => {
                        winwincode_network::DiagnosticCode::Connect
                    }
                    _ => winwincode_network::DiagnosticCode::Configuration,
                },
            ));
            failure
        })
    }

    #[must_use]
    pub const fn request_invalid() -> Self {
        Self::new(
            ProviderAdapterErrorKind::RequestInvalid,
            "Provider request is invalid",
        )
    }

    #[must_use]
    pub const fn request_translation() -> Self {
        Self::new(
            ProviderAdapterErrorKind::RequestTranslation,
            "Provider request translation failed",
        )
    }

    #[must_use]
    pub const fn request_size_limit() -> Self {
        Self::new(
            ProviderAdapterErrorKind::RequestSizeLimit,
            "Provider request exceeds its size limit",
        )
    }

    #[must_use]
    pub const fn response_content_type() -> Self {
        Self::new(
            ProviderAdapterErrorKind::ResponseContentType,
            "Provider response is not an event stream",
        )
    }

    pub(crate) fn response_content_type_status(status: u16) -> Self {
        use winwincode_network::{Acceptance, ErrorKind, NetworkFailure, Phase};
        Self {
            http_status: Some(status),
            network: Some(Box::new(NetworkFailure {
                http_status: Some(status),
                ..NetworkFailure::new(
                    ErrorKind::ProtocolInvalid,
                    Acceptance::ResponseReceived,
                    Phase::ResponseHeaders,
                )
                .with_diagnostic(winwincode_network::NetworkDiagnostic::new(
                    winwincode_network::DiagnosticCode::ContentType,
                ))
            })),
            ..Self::response_content_type()
        }
    }

    #[must_use]
    pub const fn connection() -> Self {
        Self::new(
            ProviderAdapterErrorKind::Connection,
            "Provider connection failed",
        )
    }

    #[must_use]
    pub const fn upstream() -> Self {
        Self::new(
            ProviderAdapterErrorKind::Upstream,
            "Provider returned a server error",
        )
    }

    pub(crate) const fn upstream_after(
        status: u16,
        retry_after: Option<std::time::Duration>,
    ) -> Self {
        let mut error = Self::upstream();
        error.http_status = Some(status);
        error.retry_after = retry_after;
        error
    }

    #[must_use]
    pub const fn identity_conflict() -> Self {
        Self::new(
            ProviderAdapterErrorKind::IdentityConflict,
            "Provider exchange identity conflicts",
        )
    }

    #[must_use]
    pub const fn rejected() -> Self {
        Self {
            kind: ProviderAdapterErrorKind::Rejected,
            message: "Provider rejected the request",
            http_status: None,
            retry_after: None,
            connection_pending: false,
            network: None,
        }
    }

    #[must_use]
    pub const fn rate_limited() -> Self {
        Self {
            kind: ProviderAdapterErrorKind::RateLimited,
            message: "Provider rate limit rejected the request",
            http_status: Some(429),
            retry_after: None,
            connection_pending: false,
            network: None,
        }
    }

    pub(crate) fn rate_limited_after(delay: Option<std::time::Duration>) -> Self {
        Self {
            retry_after: delay,
            ..Self::rate_limited()
        }
    }

    pub(crate) const fn retry_after(&self) -> Option<std::time::Duration> {
        self.retry_after
    }

    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            kind: ProviderAdapterErrorKind::Unavailable,
            message: "Provider adapter is unavailable",
            http_status: None,
            retry_after: None,
            connection_pending: false,
            network: None,
        }
    }

    #[must_use]
    pub const fn protocol() -> Self {
        Self {
            kind: ProviderAdapterErrorKind::Protocol,
            message: "Provider adapter response is invalid",
            http_status: None,
            retry_after: None,
            connection_pending: false,
            network: None,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderAdapterErrorKind {
        self.kind
    }

    pub(crate) const fn rejected_status(status: u16) -> Self {
        let mut error = Self::rejected();
        error.http_status = Some(status);
        error
    }

    /// Only the bounded status is retained; upstream bodies and headers are discarded.
    #[must_use]
    pub const fn http_status(&self) -> Option<u16> {
        self.http_status
    }
}

impl fmt::Display for ProviderAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProviderAdapterError {}

impl winwincode_network::RetryFailure for ProviderAdapterError {
    fn retryable(&self) -> bool {
        self.network_failure().retryable()
    }
    fn retry_after(&self) -> Option<std::time::Duration> {
        self.retry_after
    }
    fn wait_for_connection(&self) -> bool {
        self.connection_pending
    }
    fn network_failure(&self) -> winwincode_network::NetworkFailure {
        self.network_failure()
    }
}

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
            return Err(ProviderAdapterError::identity_conflict());
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

    /// Executes recovery at the complete response boundary. Non-HTTP adapters
    /// keep their own deterministic open behavior. HTTPS adapters retain every
    /// network attempt before retrying and cache only a validated completion.
    /// # Errors
    /// Rejects lost live authority, permanent failures and exhausted budgets.
    fn open_recovering(
        &self,
        invocation: &ProviderAdapterInvocation<'_>,
        credential: &ResolvedSecret,
        receipt: &ProviderGatewayOpenReceipt,
        can_start: &(dyn Fn() -> bool + Sync),
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError> {
        let _ = receipt;
        if !can_start() {
            return Err(ProviderAdapterError::from_network(
                winwincode_network::NetworkFailure::new(
                    winwincode_network::ErrorKind::AuthorityExpired,
                    winwincode_network::Acceptance::NotSent,
                    winwincode_network::Phase::Connect,
                ),
            ));
        }
        self.open(invocation, credential)
    }

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
    pub actual_cost_micros: Option<u64>,
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
        actual_cost_micros: Option<u64>,
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
            | ProviderGatewayErrorKind::AdapterConnection
            | ProviderGatewayErrorKind::AdapterUpstream
            | ProviderGatewayErrorKind::IdentityUnavailable
            | ProviderGatewayErrorKind::RouteUnavailable
            | ProviderGatewayErrorKind::AdmissionUnavailable
            | ProviderGatewayErrorKind::SettlementUnavailable
            | ProviderGatewayErrorKind::Storage => ModelAttemptFailureKind::ProviderUnavailable,
            ProviderGatewayErrorKind::AdapterProtocol
            | ProviderGatewayErrorKind::AdapterRequestTranslation
            | ProviderGatewayErrorKind::AdapterResponseContentType
            | ProviderGatewayErrorKind::AdapterIdentityConflict => {
                ModelAttemptFailureKind::Protocol
            }
            ProviderGatewayErrorKind::AdapterRejected
            | ProviderGatewayErrorKind::AdapterRequestInvalid
            | ProviderGatewayErrorKind::AdapterRequestSizeLimit
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
    pub cost_micros: Option<u64>,
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

#[cfg(test)]
mod network_error_tests {
    use super::*;
    use winwincode_network::{Acceptance, ErrorKind, Phase};

    #[test]
    fn protocol_retry_preserves_local_rejections_and_identity_fences() {
        for (error, kind) in [
            (
                ProviderAdapterError::request_invalid(),
                ErrorKind::RequestInvalid,
            ),
            (
                ProviderAdapterError::request_translation(),
                ErrorKind::RequestInvalid,
            ),
            (
                ProviderAdapterError::request_size_limit(),
                ErrorKind::RequestInvalid,
            ),
            (ProviderAdapterError::rejected(), ErrorKind::RequestInvalid),
            (
                ProviderAdapterError::identity_conflict(),
                ErrorKind::IntegrityInvalid,
            ),
            (
                ProviderAdapterError::unavailable(),
                ErrorKind::StorageUnavailable,
            ),
        ] {
            assert_eq!(error.network_failure().kind, kind);
            assert!(!error.network_failure().retryable());
        }
    }

    #[test]
    fn wrong_content_type_retains_received_http_status_and_retries() {
        let error = ProviderAdapterError::response_content_type_status(200);
        assert_eq!(error.kind(), ProviderAdapterErrorKind::ResponseContentType);
        let failure = error.network_failure();
        assert_eq!(failure.kind, ErrorKind::ProtocolInvalid);
        assert_eq!(failure.acceptance, Acceptance::ResponseReceived);
        assert_eq!(failure.phase, Phase::ResponseHeaders);
        assert_eq!(failure.http_status, Some(200));
        assert!(failure.retryable());
    }
}
