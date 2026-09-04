// SPDX-License-Identifier: Apache-2.0

//! Central, secret-safe Provider request routing.
//!
//! The Gateway validates the Worker envelope, resolves the configured
//! [`ModelRoute`], selects one exact Provider adapter, and settles terminal
//! outcomes once. Secret bytes exist only for the duration of the adapter
//! call. The Gateway owns no Codex conversation state and emits no durable
//! events; streaming conversion belongs to the `ModelPort` layer.

use std::{collections::BTreeMap, fmt};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};
use sha2::{Digest, Sha256};
use winwincode_api::generated::ModelRoute;
use winwincode_domain::{
    Instant, ModelExchangeId, ProductSessionId, RequestId, SchemaVersion, SessionIdentity,
    Sha256Digest, UserId, WorkerSessionId,
};
use winwincode_execution_port::generated::{
    EncodedPayload, ExecutionLeaseStamp, ExecutionPortErrorCode, LeaseWriteStatus, ModelAckMessage,
    ModelOpenMessage,
};
use winwincode_storage::{ProductStateStorage, StorageError};

use crate::{
    CredentialLeakError, CredentialLeakGate, CredentialOutputBoundary, CredentialReferenceError,
    CredentialReferenceErrorKind, CredentialReferenceService, EnterpriseQuotaAdmissionPort,
    FrozenModelRouteAuthority, ModelAttemptCharge, ModelAttemptFailureFact,
    ModelAttemptFailureKind, ModelExecutionCertainty, ModelReservationReleaseReason,
    ModelReservationTerminalReceipt, ModelRetrySettlementContextPort, ModelSettingsError,
    ModelSettingsErrorKind, ModelSettingsService, ModelSettingsTarget, ProviderAdmissionError,
    ProviderAdmissionErrorKind, ProviderAdmissionOpenRequest, ProviderCatalogError,
    ProviderCatalogErrorKind, ProviderCatalogService, ProviderEnterpriseQuotaOpen,
    ProviderEnterpriseQuotaSaga, ProviderGatewayAdmissionPort, ProviderOperationalAdmissionError,
    ProviderOperationalAdmissionPort, ProviderTokenUsage, ResolvedSecret, SecretStoreError,
    SecretStorePort,
};

const MAX_PROVIDER_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const MODEL_CANCELLATION_MESSAGE: &str = "model exchange cancelled by Worker";

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
    PolicyDenied,
    PolicyUnavailable,
    AdmissionDenied,
    AdmissionUnavailable,
    SettlementUnavailable,
    CredentialLeak,
    Storage,
}

/// Bounded Gateway error which never retains Provider diagnostics or secrets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderGatewayError {
    kind: ProviderGatewayErrorKind,
    message: &'static str,
}

impl ProviderGatewayError {
    const fn new(kind: ProviderGatewayErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    pub(crate) const fn storage() -> Self {
        Self::new(
            ProviderGatewayErrorKind::Storage,
            "Provider Gateway durable progress operation failed",
        )
    }

    pub(crate) const fn policy_denied() -> Self {
        Self::new(
            ProviderGatewayErrorKind::PolicyDenied,
            "Provider enterprise Policy denied the request",
        )
    }

    pub(crate) const fn policy_unavailable() -> Self {
        Self::new(
            ProviderGatewayErrorKind::PolicyUnavailable,
            "Provider enterprise Policy authority is unavailable",
        )
    }

    pub(crate) const fn invalid() -> Self {
        Self::new(
            ProviderGatewayErrorKind::InvalidRequest,
            "Provider Gateway request is invalid",
        )
    }

    pub(crate) const fn exchange_not_found() -> Self {
        Self::new(
            ProviderGatewayErrorKind::ExchangeNotFound,
            "Provider Gateway exchange was not found",
        )
    }

    pub(crate) const fn terminal_conflict() -> Self {
        Self::new(
            ProviderGatewayErrorKind::TerminalConflict,
            "Provider Gateway terminal command conflicts with durable state",
        )
    }

    const fn credential_unavailable() -> Self {
        Self::new(
            ProviderGatewayErrorKind::CredentialUnavailable,
            "Provider Credential is unavailable",
        )
    }

    /// Returns the stable machine-readable category.
    #[must_use]
    pub const fn kind(&self) -> ProviderGatewayErrorKind {
        self.kind
    }
}

impl fmt::Display for ProviderGatewayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProviderGatewayError {}

impl From<CredentialLeakError> for ProviderGatewayError {
    fn from(_error: CredentialLeakError) -> Self {
        Self::new(
            ProviderGatewayErrorKind::CredentialLeak,
            "Provider request was rejected by the Credential leak gate",
        )
    }
}

impl From<StorageError> for ProviderGatewayError {
    fn from(_error: StorageError) -> Self {
        Self::new(
            ProviderGatewayErrorKind::Storage,
            "Provider Gateway storage operation failed",
        )
    }
}

/// Trusted routing target returned after Worker identity and lease validation.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderGatewayIdentity {
    target: ModelSettingsTarget,
    user_id: Option<UserId>,
}

impl ProviderGatewayIdentity {
    /// Constructs the only Gateway target accepted for a model-open envelope.
    #[must_use]
    pub const fn product_session(
        repository_scope: winwincode_api::generated::RepositoryScope,
        product_session_id: ProductSessionId,
    ) -> Self {
        Self {
            target: ModelSettingsTarget::ProductSession {
                repository_scope,
                product_session_id,
            },
            user_id: None,
        }
    }

    #[must_use]
    pub const fn product_session_for_user(
        repository_scope: winwincode_api::generated::RepositoryScope,
        product_session_id: ProductSessionId,
        user_id: UserId,
    ) -> Self {
        Self {
            target: ModelSettingsTarget::ProductSession {
                repository_scope,
                product_session_id,
            },
            user_id: Some(user_id),
        }
    }

    /// Returns the trusted target used for settings resolution.
    #[must_use]
    pub const fn target(&self) -> &ModelSettingsTarget {
        &self.target
    }

    #[must_use]
    pub const fn user_id(&self) -> Option<&UserId> {
        self.user_id.as_ref()
    }
}

/// Stable identity validation failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderGatewayIdentityErrorKind {
    Denied,
    Unavailable,
}

/// Secret-free identity port failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderGatewayIdentityError {
    kind: ProviderGatewayIdentityErrorKind,
    message: &'static str,
}

impl ProviderGatewayIdentityError {
    #[must_use]
    pub const fn denied() -> Self {
        Self {
            kind: ProviderGatewayIdentityErrorKind::Denied,
            message: "Provider Gateway identity was denied",
        }
    }

    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            kind: ProviderGatewayIdentityErrorKind::Unavailable,
            message: "Provider Gateway identity service is unavailable",
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderGatewayIdentityErrorKind {
        self.kind
    }
}

impl fmt::Display for ProviderGatewayIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProviderGatewayIdentityError {}

/// Validates the authoritative Worker session, lease, and fencing identity.
pub trait ProviderGatewayIdentityPort: Send + Sync {
    /// Returns a trusted settings target for one exact open envelope.
    ///
    /// # Errors
    ///
    /// Returns only stable denial or dependency-unavailable categories.
    fn authorize(
        &self,
        message: &ModelOpenMessage,
    ) -> Result<ProviderGatewayIdentity, ProviderGatewayIdentityError>;
}

/// Provider-neutral request exposed to exactly one selected adapter.
///
/// Serialization is intentionally absent and Debug always redacts the body.
pub struct ProviderAdapterInvocation<'a> {
    model_exchange_id: &'a ModelExchangeId,
    request_id: &'a RequestId,
    adapter_request_id: &'a str,
    model_id: &'a str,
    content_type: &'a str,
    payload: &'a [u8],
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
        validate_token(&adapter_request_id, 200).map_err(|_| ProviderAdapterError::protocol())?;
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

/// Ordered durable checkpoint around each terminal external side effect.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ProviderGatewayTerminalProgressStage {
    Prepared,
    CancelStarted,
    Cancelled,
    ReleaseStarted,
    Released,
    AdmissionStarted,
    AdmissionSettled,
    SettlementStarted,
    SettlementSettled,
}

/// Rehydratable secret-free terminal saga checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderGatewayTerminalProgress {
    pub stage: ProviderGatewayTerminalProgressStage,
    pub admission: Option<ModelReservationTerminalReceipt>,
    pub terminal: Option<ProviderGatewayTerminalReceipt>,
}

/// Durable terminal progress authority used by the production runtime.
pub trait ProviderGatewayTerminalProgressPort: Send + Sync {
    /// Loads the current checkpoint for an exact exchange.
    ///
    /// # Errors
    ///
    /// Returns a stable error for corrupt or unavailable durable state.
    fn load(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ProviderGatewayTerminalProgress>, ProviderGatewayError>;

    /// Records one checkpoint before or after an external side effect.
    ///
    /// # Errors
    ///
    /// Rejects changed commands, invalid ordering, or unavailable storage.
    fn record(
        &self,
        model_exchange_id: &ModelExchangeId,
        command: ProviderGatewayTerminal,
        stage: ProviderGatewayTerminalProgressStage,
        admission: Option<&ModelReservationTerminalReceipt>,
        terminal: Option<&ProviderGatewayTerminalReceipt>,
        observed_at: &Instant,
    ) -> Result<(), ProviderGatewayError>;
}

/// Idempotent result of one Provider transport control transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderStreamControlReceipt {
    pub action: ProviderStreamControlAction,
    pub replayed: bool,
}

/// Provider-neutral terminal outcome.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderGatewayTerminalOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

/// Safe terminal facts sent to the owner of billing or pool settlement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderGatewaySettlement {
    pub model_exchange_id: ModelExchangeId,
    pub request_id: RequestId,
    pub provider_id: String,
    pub model_id: String,
    pub adapter_request_id: String,
    pub settled_at: Instant,
    pub outcome: ProviderGatewayTerminalOutcome,
    pub admission_terminal: ModelReservationTerminalReceipt,
    pub failure: Option<ModelAttemptFailureFact>,
    pub charge: Option<ModelAttemptCharge>,
}

impl Serialize for ProviderGatewaySettlement {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("ProviderGatewaySettlement", 10)?;
        state.serialize_field("modelExchangeId", &self.model_exchange_id)?;
        state.serialize_field("requestId", &self.request_id)?;
        state.serialize_field("providerId", &self.provider_id)?;
        state.serialize_field("modelId", &self.model_id)?;
        state.serialize_field("adapterRequestId", &self.adapter_request_id)?;
        state.serialize_field("settledAt", &self.settled_at)?;
        state.serialize_field("outcome", &self.outcome)?;
        state.serialize_field("admissionTerminal", &self.admission_terminal)?;
        state.serialize_field("failure", &self.failure)?;
        state.serialize_field("charge", &SerializableAttemptCharge(self.charge.as_ref()))?;
        state.end()
    }
}

struct SerializableAttemptCharge<'a>(Option<&'a ModelAttemptCharge>);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SerializableAttemptChargeValue<'a> {
    provider_usage_id: &'a str,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    cost_micros: u64,
}

impl Serialize for SerializableAttemptCharge<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let Some(charge) = self.0 else {
            return serializer.serialize_none();
        };
        serializer.serialize_some(&SerializableAttemptChargeValue {
            provider_usage_id: &charge.provider_usage_id,
            input_tokens: charge.usage.input_tokens,
            cached_input_tokens: charge.usage.cached_input_tokens,
            cache_write_input_tokens: charge.usage.cache_write_input_tokens,
            output_tokens: charge.usage.output_tokens,
            reasoning_output_tokens: charge.usage.reasoning_output_tokens,
            cost_micros: charge.cost_micros,
        })
    }
}

/// Stable settlement dependency error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderGatewaySettlementError;

impl fmt::Display for ProviderGatewaySettlementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Provider Gateway settlement service is unavailable")
    }
}

impl std::error::Error for ProviderGatewaySettlementError {}

/// Settles one exchange. `model_exchange_id` is its idempotency identity.
pub trait ProviderGatewaySettlementPort: Send + Sync {
    /// Settles terminal facts.
    ///
    /// # Errors
    ///
    /// The Gateway leaves the exchange retryable when settlement fails.
    fn settle(
        &self,
        settlement: &ProviderGatewaySettlement,
    ) -> Result<(), ProviderGatewaySettlementError>;
}

/// Secret-free result of opening an exchange.
pub struct ProviderGatewayOpenReceipt {
    pub model_exchange_id: ModelExchangeId,
    pub request_id: RequestId,
    pub route: ModelRoute,
    pub adapter_request_id: String,
    pub idempotent_replay: bool,
    stream_leak_gate: CredentialLeakGate,
}

impl ProviderGatewayOpenReceipt {
    pub(crate) fn stream_leak_gate(&self) -> CredentialLeakGate {
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

/// Secret-free durable Gateway exchange authority used only for crash restore.
#[derive(Clone)]
pub struct ProviderGatewayDurableExchange {
    gateway_open_digest: [u8; 32],
    open_receipt: ProviderGatewayOpenReceipt,
    route_authority: FrozenModelRouteAuthority,
    lease: ExecutionLeaseStamp,
    worker_session_id: WorkerSessionId,
    session_identity: SessionIdentity,
}

impl ProviderGatewayDurableExchange {
    /// Returns the exact frozen route authority admitted before Provider open.
    #[must_use]
    pub const fn route_authority(&self) -> &FrozenModelRouteAuthority {
        &self.route_authority
    }

    /// Returns the secret-free open receipt.
    #[must_use]
    pub const fn open_receipt(&self) -> &ProviderGatewayOpenReceipt {
        &self.open_receipt
    }

    /// Returns the original exact execution lease.
    #[must_use]
    pub const fn lease(&self) -> &ExecutionLeaseStamp {
        &self.lease
    }

    /// Returns the original Worker session identity.
    #[must_use]
    pub const fn worker_session_id(&self) -> &WorkerSessionId {
        &self.worker_session_id
    }

    /// Returns the original product session identity.
    #[must_use]
    pub const fn session_identity(&self) -> &SessionIdentity {
        &self.session_identity
    }

    /// Encodes canonical secret-free receipt, lease, and session facts.
    ///
    /// The frozen route authority is persisted separately through its narrow
    /// durable API, so Credential reference metadata has one authority copy.
    ///
    /// # Errors
    ///
    /// Rejects an inconsistent snapshot or serialization failure.
    pub fn to_durable_receipt_json(&self) -> Result<Vec<u8>, ProviderGatewayError> {
        validate_durable_exchange(self)?;
        serde_json::to_vec(&DurableGatewayOpenReceipt::from_exchange(self)?)
            .map_err(|_| ProviderGatewayError::invalid())
    }

    /// Rehydrates canonical secret-free receipt, lease, and session facts.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical JSON, a changed frozen authority, or malformed
    /// receipt identity.
    pub fn from_durable_parts(
        route_authority: FrozenModelRouteAuthority,
        receipt_json: &[u8],
    ) -> Result<Self, ProviderGatewayError> {
        route_authority
            .validate_fingerprint()
            .map_err(|_| ProviderGatewayError::invalid())?;
        let stored: DurableGatewayOpenReceipt =
            serde_json::from_slice(receipt_json).map_err(|_| ProviderGatewayError::invalid())?;
        if serde_json::to_vec(&stored).map_err(|_| ProviderGatewayError::invalid())? != receipt_json
        {
            return Err(ProviderGatewayError::invalid());
        }
        let gateway_open_digest = parse_sha256(&stored.gateway_open_digest)?;
        let stream_leak_gate = CredentialLeakGate::from_durable_fingerprint_json(
            stored.leak_fingerprint_json.as_bytes(),
        )
        .map_err(|_| ProviderGatewayError::invalid())?;
        let exchange = Self {
            gateway_open_digest,
            open_receipt: ProviderGatewayOpenReceipt {
                model_exchange_id: stored.model_exchange_id,
                request_id: stored.request_id,
                route: stored.route,
                adapter_request_id: stored.adapter_request_id,
                idempotent_replay: true,
                stream_leak_gate,
            },
            route_authority,
            lease: stored.lease,
            worker_session_id: stored.worker_session_id,
            session_identity: stored.session_identity,
        };
        validate_durable_exchange(&exchange)?;
        Ok(exchange)
    }
}

impl fmt::Debug for ProviderGatewayDurableExchange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderGatewayDurableExchange")
            .field("model_exchange_id", &self.open_receipt.model_exchange_id)
            .field("request_id", &self.open_receipt.request_id)
            .field("route", &self.open_receipt.route)
            .field("route_authority", &self.route_authority)
            .field("lease", &self.lease)
            .field("worker_session_id", &self.worker_session_id)
            .field("session_identity", &self.session_identity)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableGatewayOpenReceipt {
    gateway_open_digest: Sha256Digest,
    model_exchange_id: ModelExchangeId,
    request_id: RequestId,
    route: ModelRoute,
    adapter_request_id: String,
    leak_fingerprint_json: String,
    lease: ExecutionLeaseStamp,
    worker_session_id: WorkerSessionId,
    session_identity: SessionIdentity,
}

impl DurableGatewayOpenReceipt {
    fn from_exchange(
        exchange: &ProviderGatewayDurableExchange,
    ) -> Result<Self, ProviderGatewayError> {
        Ok(Self {
            gateway_open_digest: Sha256Digest(format!(
                "sha256:{}",
                lower_hex(&exchange.gateway_open_digest)
            )),
            model_exchange_id: exchange.open_receipt.model_exchange_id.clone(),
            request_id: exchange.open_receipt.request_id.clone(),
            route: exchange.open_receipt.route.clone(),
            adapter_request_id: exchange.open_receipt.adapter_request_id.clone(),
            leak_fingerprint_json: String::from_utf8(
                exchange
                    .open_receipt
                    .stream_leak_gate
                    .to_durable_fingerprint_json()
                    .map_err(|_| ProviderGatewayError::invalid())?,
            )
            .map_err(|_| ProviderGatewayError::invalid())?,
            lease: exchange.lease.clone(),
            worker_session_id: exchange.worker_session_id.clone(),
            session_identity: exchange.session_identity.clone(),
        })
    }
}

/// Result of one terminal settlement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderGatewayTerminalReceipt {
    pub model_exchange_id: ModelExchangeId,
    pub outcome: ProviderGatewayTerminalOutcome,
    pub admission: ModelReservationTerminalReceipt,
    pub settled_at: Instant,
    pub idempotent_replay: bool,
}

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

    const fn charge(self) -> Option<ProviderGatewayTerminalCharge> {
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

#[derive(Clone)]
struct ProviderReadProgress {
    paused: bool,
    synchronized: bool,
}

#[derive(Clone)]
struct ProviderTerminalEffects {
    cancelled: bool,
    released: bool,
}

#[derive(Clone)]
struct ExchangeRecord {
    open_digest: [u8; 32],
    open_receipt: ProviderGatewayOpenReceipt,
    admission_authority: FrozenModelRouteAuthority,
    lease: ExecutionLeaseStamp,
    worker_session_id: WorkerSessionId,
    session_identity: SessionIdentity,
    provider_read: ProviderReadProgress,
    provider_terminal: ProviderTerminalEffects,
    terminating: Option<ProviderGatewayTerminal>,
    terminal: Option<(ProviderGatewayTerminal, ProviderGatewayTerminalReceipt)>,
}

struct GatewayQuotaOperational<'gateway, 'storage> {
    gateway: &'gateway mut ProviderGateway<'storage>,
    message: &'gateway ModelOpenMessage,
    reservation: &'gateway crate::ProviderAdmissionOpenReceipt,
    adapter_request_id: &'gateway str,
}

impl ProviderOperationalAdmissionPort for GatewayQuotaOperational<'_, '_> {
    type Receipt = ProviderGatewayOpenReceipt;

    fn reserve(&mut self) -> Result<Self::Receipt, ProviderOperationalAdmissionError> {
        self.gateway
            .open_after_reservation(self.message, self.reservation, self.adapter_request_id)
            .map_err(|error| match error.kind() {
                ProviderGatewayErrorKind::AdmissionDenied => {
                    ProviderOperationalAdmissionError::Denied
                }
                _ => ProviderOperationalAdmissionError::Unavailable,
            })
    }
}

struct ResolvedProviderOpen {
    route: ModelRoute,
    reference: crate::CredentialReferenceResolution,
    admission: crate::ProviderAdmissionOpenReceipt,
}

struct ResolvedRouteContext {
    route: ModelRoute,
    credential_scope: winwincode_api::generated::Scope,
    settings: crate::ModelSettingsProjection,
    capability: crate::ResolvedModelCapability,
    provider_account_selected: bool,
}

fn admission_is_exact_replay(
    actual: &crate::ProviderAdmissionOpenReceipt,
    expected: &crate::ProviderAdmissionOpenReceipt,
) -> bool {
    let mut actual_reservation = actual.reservation.clone();
    let mut expected_reservation = expected.reservation.clone();
    actual_reservation.idempotent_replay = false;
    expected_reservation.idempotent_replay = false;
    actual.reservation.idempotent_replay
        && actual.route_authority == expected.route_authority
        && actual_reservation == expected_reservation
}

/// Central request router. Exchange bookkeeping is process-local and contains
/// only safe identifiers and digests; no request or Credential bytes are kept.
pub struct ProviderGateway<'a> {
    storage: &'a mut dyn ProductStateStorage,
    secret_store: &'a dyn SecretStorePort,
    identity: &'a dyn ProviderGatewayIdentityPort,
    settlement: &'a dyn ProviderGatewaySettlementPort,
    admission: &'a mut dyn ProviderGatewayAdmissionPort,
    adapters: BTreeMap<String, Box<dyn ProviderAdapterPort>>,
    exchanges: BTreeMap<String, ExchangeRecord>,
}

impl<'a> ProviderGateway<'a> {
    #[must_use]
    pub fn new(
        storage: &'a mut dyn ProductStateStorage,
        secret_store: &'a dyn SecretStorePort,
        identity: &'a dyn ProviderGatewayIdentityPort,
        settlement: &'a dyn ProviderGatewaySettlementPort,
        admission: &'a mut dyn ProviderGatewayAdmissionPort,
    ) -> Self {
        Self {
            storage,
            secret_store,
            identity,
            settlement,
            admission,
            adapters: BTreeMap::new(),
            exchanges: BTreeMap::new(),
        }
    }

    /// Registers exactly one adapter for its canonical Provider identifier.
    ///
    /// # Errors
    ///
    /// Rejects an invalid or duplicate Provider identifier.
    pub fn register_adapter(
        &mut self,
        adapter: Box<dyn ProviderAdapterPort>,
    ) -> Result<(), ProviderGatewayError> {
        let provider_id = adapter.provider_id();
        validate_token(provider_id, 128)?;
        if self.adapters.contains_key(provider_id) {
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::AdapterProtocol,
                "Provider adapter is already registered",
            ));
        }
        self.adapters.insert(provider_id.to_owned(), adapter);
        Ok(())
    }

    /// Copies the secret-free authority required to restore one open exchange.
    ///
    /// # Errors
    ///
    /// Rejects an unknown, terminating, terminal, or corrupt exchange.
    pub fn durable_exchange(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<ProviderGatewayDurableExchange, ProviderGatewayError> {
        let record = self.exchange(model_exchange_id)?;
        if record.terminating.is_some() || record.terminal.is_some() {
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::TerminalConflict,
                "Only an open Provider exchange can be snapshotted",
            ));
        }
        let exchange = ProviderGatewayDurableExchange {
            gateway_open_digest: record.open_digest,
            open_receipt: record.open_receipt.clone(),
            route_authority: record.admission_authority.clone(),
            lease: record.lease.clone(),
            worker_session_id: record.worker_session_id.clone(),
            session_identity: record.session_identity.clone(),
        };
        validate_durable_exchange(&exchange)?;
        Ok(exchange)
    }

    /// Restores one already accepted durable exchange without resolving a
    /// Credential or invoking a Provider adapter.
    ///
    /// # Errors
    ///
    /// Rejects malformed authority, an unregistered Provider, or a changed
    /// in-memory exchange with the same identity.
    pub fn restore_durable_exchange(
        &mut self,
        exchange: &ProviderGatewayDurableExchange,
    ) -> Result<ProviderGatewayOpenReceipt, ProviderGatewayError> {
        validate_durable_exchange(exchange)?;
        self.adapter(&exchange.open_receipt.route.provider_id)?;
        if let Some(existing) = self
            .exchanges
            .get(&exchange.open_receipt.model_exchange_id.0)
        {
            if existing.open_digest != exchange.gateway_open_digest
                || existing.open_receipt.model_exchange_id
                    != exchange.open_receipt.model_exchange_id
                || existing.open_receipt.request_id != exchange.open_receipt.request_id
                || existing.open_receipt.route != exchange.open_receipt.route
                || existing.open_receipt.adapter_request_id
                    != exchange.open_receipt.adapter_request_id
                || existing.admission_authority != exchange.route_authority
                || existing.lease != exchange.lease
                || existing.worker_session_id != exchange.worker_session_id
                || existing.session_identity != exchange.session_identity
            {
                return Err(ProviderGatewayError::new(
                    ProviderGatewayErrorKind::ExchangeConflict,
                    "Durable Provider exchange conflicts with in-memory state",
                ));
            }
            let mut replay = existing.open_receipt.clone();
            replay.idempotent_replay = true;
            return Ok(replay);
        }
        let mut receipt = exchange.open_receipt.clone();
        receipt.idempotent_replay = true;
        self.exchanges.insert(
            receipt.model_exchange_id.0.clone(),
            ExchangeRecord {
                open_digest: exchange.gateway_open_digest,
                open_receipt: receipt.clone(),
                admission_authority: exchange.route_authority.clone(),
                lease: exchange.lease.clone(),
                worker_session_id: exchange.worker_session_id.clone(),
                session_identity: exchange.session_identity.clone(),
                provider_read: ProviderReadProgress {
                    paused: false,
                    synchronized: false,
                },
                provider_terminal: ProviderTerminalEffects {
                    cancelled: false,
                    released: false,
                },
                terminating: None,
                terminal: None,
            },
        );
        Ok(receipt)
    }

    /// Validates and routes one Worker model request.
    ///
    /// The Worker-supplied route must exactly equal the current configured
    /// route for the system account source. Account-backed sessions replace
    /// that secret-free hint with their server-authorized account route. Secret
    /// bytes are resolved only after route and payload checks and are borrowed
    /// only by the selected adapter call.
    ///
    /// # Errors
    ///
    /// Returns stable Gateway categories without Provider or secret text.
    pub fn open(
        &mut self,
        message: &ModelOpenMessage,
        worker_route: &ModelRoute,
        adapter_request_id: &str,
    ) -> Result<ProviderGatewayOpenReceipt, ProviderGatewayError> {
        self.open_inner(message, worker_route, adapter_request_id, None)
    }

    /// Opens only after exact replay of a previously durable reservation.
    ///
    /// # Errors
    ///
    /// Rejects a changed or newly-created admission before resolving a secret
    /// or invoking a Provider adapter, in addition to [`Self::open`] errors.
    pub fn open_after_reservation(
        &mut self,
        message: &ModelOpenMessage,
        reservation: &crate::ProviderAdmissionOpenReceipt,
        adapter_request_id: &str,
    ) -> Result<ProviderGatewayOpenReceipt, ProviderGatewayError> {
        self.open_inner(
            message,
            reservation.route_authority.route(),
            adapter_request_id,
            Some(reservation),
        )
    }

    fn open_inner(
        &mut self,
        message: &ModelOpenMessage,
        worker_route: &ModelRoute,
        adapter_request_id: &str,
        expected_reservation: Option<&crate::ProviderAdmissionOpenReceipt>,
    ) -> Result<ProviderGatewayOpenReceipt, ProviderGatewayError> {
        validate_open_envelope(message)?;
        validate_model_route(worker_route)?;
        validate_token(adapter_request_id, 200)?;

        let identity = self
            .identity
            .authorize(message)
            .map_err(|error| map_identity_error(&error))?;
        validate_identity(message, &identity)?;

        let open_digest =
            gateway_open_digest(message, worker_route, identity.target(), adapter_request_id)?;
        if let Some(record) = self.exchanges.get(&message.model_exchange_id.0) {
            if record.open_digest != open_digest {
                return Err(ProviderGatewayError::new(
                    ProviderGatewayErrorKind::ExchangeConflict,
                    "Model exchange identity was reused with different input",
                ));
            }
            if expected_reservation
                .is_some_and(|expected| record.admission_authority != expected.route_authority)
            {
                return Err(ProviderGatewayError::new(
                    ProviderGatewayErrorKind::ExchangeConflict,
                    "Provider reservation authority changed after open",
                ));
            }
            let mut receipt = record.open_receipt.clone();
            receipt.idempotent_replay = true;
            return Ok(receipt);
        }

        let payload = decode_payload(&message.request)?;
        let resolved = self.resolve_admitted_open(message, Some(worker_route), &identity)?;
        if expected_reservation
            .is_some_and(|expected| !admission_is_exact_replay(&resolved.admission, expected))
        {
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::ExchangeConflict,
                "Provider admission did not replay the pre-open reservation",
            ));
        }
        let (adapter_receipt, leak_gate) = match self.invoke_provider(
            message,
            &payload,
            &resolved,
            adapter_request_id,
        ) {
            Ok(result) => result,
            Err(error) => {
                let settlement = if matches!(
                    error.kind(),
                    ProviderGatewayErrorKind::AdapterRateLimited
                        | ProviderGatewayErrorKind::AdapterUnavailable
                        | ProviderGatewayErrorKind::CredentialUnavailable
                ) {
                    crate::provider_account::ProviderAccountExchangeSettlement::RetryableBeforeAcceptance
                } else {
                    crate::provider_account::ProviderAccountExchangeSettlement::Final
                };
                self.release_provider_account_route(
                    &identity,
                    &message.model_exchange_id,
                    0,
                    settlement,
                )?;
                return Err(error);
            }
        };
        let receipt = ProviderGatewayOpenReceipt {
            model_exchange_id: message.model_exchange_id.clone(),
            request_id: message.request_id.clone(),
            route: resolved.route,
            adapter_request_id: adapter_receipt.adapter_request_id,
            idempotent_replay: resolved.admission.reservation.idempotent_replay,
            stream_leak_gate: leak_gate,
        };
        self.exchanges.insert(
            message.model_exchange_id.0.clone(),
            ExchangeRecord {
                open_digest,
                open_receipt: receipt.clone(),
                admission_authority: resolved.admission.route_authority,
                lease: message.lease.clone(),
                worker_session_id: message.worker_session_id.clone(),
                session_identity: message.session_identity.clone(),
                provider_read: ProviderReadProgress {
                    paused: false,
                    synchronized: true,
                },
                provider_terminal: ProviderTerminalEffects {
                    cancelled: false,
                    released: false,
                },
                terminating: None,
                terminal: None,
            },
        );
        Ok(receipt)
    }

    /// Replays the durable enterprise quota request before resolving a secret
    /// or invoking the existing Provider adapter.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable, denied, or terminal enterprise quota result
    /// before any Provider side effect.
    pub fn open_after_reservation_with_enterprise_quota(
        &mut self,
        message: &ModelOpenMessage,
        reservation: &crate::ProviderAdmissionOpenReceipt,
        adapter_request_id: &str,
        contexts: &dyn ModelRetrySettlementContextPort,
        enterprise_quota: &mut dyn EnterpriseQuotaAdmissionPort,
    ) -> Result<ProviderGatewayOpenReceipt, ProviderGatewayError> {
        let mut operational = GatewayQuotaOperational {
            gateway: self,
            message,
            reservation,
            adapter_request_id,
        };
        match ProviderEnterpriseQuotaSaga::new(enterprise_quota).reserve_durable_then_admit(
            contexts,
            &message.model_exchange_id,
            &mut operational,
        ) {
            Ok(ProviderEnterpriseQuotaOpen::Admitted { operational, .. }) => Ok(operational),
            Ok(
                ProviderEnterpriseQuotaOpen::Denied
                | ProviderEnterpriseQuotaOpen::TerminalReplay(_),
            ) => Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::AdmissionDenied,
                "Provider enterprise quota did not admit this exchange",
            )),
            Err(_) => Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::AdmissionUnavailable,
                "Provider enterprise quota operation failed",
            )),
        }
    }

    /// Durably reserves the current route before retry context persistence.
    ///
    /// This step reads only secret-free settings, catalog, and Credential
    /// reference authority. It neither decodes the model payload nor resolves
    /// a secret or invokes a Provider adapter. [`Self::open`] must later replay
    /// this exact reservation with the returned route authority.
    ///
    /// # Errors
    ///
    /// Rejects invalid Worker authority, unavailable route metadata, or a
    /// denied/conflicting durable admission reservation.
    pub fn reserve_before_open(
        &mut self,
        message: &ModelOpenMessage,
    ) -> Result<crate::ProviderAdmissionOpenReceipt, ProviderGatewayError> {
        validate_open_envelope(message)?;
        let identity = self
            .identity
            .authorize(message)
            .map_err(|error| map_identity_error(&error))?;
        validate_identity(message, &identity)?;
        self.resolve_admitted_open(message, None, &identity)
            .map(|resolved| resolved.admission)
    }

    fn resolve_admitted_open(
        &mut self,
        message: &ModelOpenMessage,
        worker_route: Option<&ModelRoute>,
        identity: &ProviderGatewayIdentity,
    ) -> Result<ResolvedProviderOpen, ProviderGatewayError> {
        let context = self.resolve_route_context(message, identity)?;
        let route = context.route;
        let credential_scope = context.credential_scope;
        if !context.provider_account_selected
            && worker_route.is_some_and(|worker_route| &route != worker_route)
        {
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::RouteMismatch,
                "Worker route does not match the configured model route",
            ));
        }
        self.adapter(&route.provider_id)?;
        let settings = context.settings;
        if settings.default_model_route.as_ref() != Some(&route) {
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::RouteUnavailable,
                "Model settings changed during Provider route resolution",
            ));
        }
        let capability = context.capability;
        let reference = CredentialReferenceService::new(self.storage)
            .resolve(&credential_scope, &route.credential_reference_id)
            .map_err(|error| map_credential_reference_error(&error))?;
        if reference.provider_id() != route.provider_id {
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::CredentialScopeMismatch,
                "Credential reference does not belong to the configured Provider",
            ));
        }
        let admission_result = self
            .admission
            .reserve(&ProviderAdmissionOpenRequest {
                identity,
                settings: &settings,
                capability: &capability,
                credential: &reference,
                message,
            })
            .map_err(|error| map_admission_reserve_error(&error));
        let admission = match admission_result {
            Ok(admission) => admission,
            Err(error) => {
                if context.provider_account_selected {
                    self.release_provider_account_route(
                        identity,
                        &message.model_exchange_id,
                        0,
                        crate::provider_account::ProviderAccountExchangeSettlement::Final,
                    )?;
                }
                return Err(error);
            }
        };
        if !admission.reservation.admitted() {
            if context.provider_account_selected {
                self.release_provider_account_route(
                    identity,
                    &message.model_exchange_id,
                    0,
                    crate::provider_account::ProviderAccountExchangeSettlement::Final,
                )?;
            }
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::AdmissionDenied,
                "Provider request was denied by durable model admission",
            ));
        }
        Ok(ResolvedProviderOpen {
            route,
            reference,
            admission,
        })
    }

    #[allow(clippy::too_many_lines)] // Keeps both account and system authority resolution auditable.
    fn resolve_route_context(
        &mut self,
        message: &ModelOpenMessage,
        identity: &ProviderGatewayIdentity,
    ) -> Result<ResolvedRouteContext, ProviderGatewayError> {
        let ModelSettingsTarget::ProductSession {
            repository_scope,
            product_session_id,
        } = identity.target()
        else {
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::IdentityDenied,
                "Provider Gateway target is not a ProductSession",
            ));
        };
        let selection = crate::product_session_service::load_product_session_model_selection(
            self.storage,
            repository_scope,
            product_session_id,
        );
        let selection = match selection {
            Ok(selection) => Some(selection),
            Err(error)
                if error.code()
                    == crate::product_session_service::ProductSessionServiceErrorCode::NotFound
                    && identity.user_id().is_none() =>
            {
                None
            }
            Err(error)
                if error.code()
                    == crate::product_session_service::ProductSessionServiceErrorCode::NotFound
                    && identity.user_id().is_some()
                    && message.session_identity.stage_run_id.is_some() =>
            {
                let (system_route, _) = ModelSettingsService::new(self.storage)
                    .resolve_with_catalog_scope(identity.target())
                    .map_err(|error| map_model_settings_error(&error))?;
                crate::provider_account::ProviderAccountRoutingService::new(self.storage)
                    .default_selection_for_user(
                        identity
                            .user_id()
                            .ok_or_else(ProviderGatewayError::invalid)?,
                        repository_scope,
                        &system_route.model_id,
                        &message.sent_at,
                    )
                    .map_err(map_provider_account_route_error)?
            }
            Err(_) => {
                return Err(ProviderGatewayError::new(
                    ProviderGatewayErrorKind::RouteUnavailable,
                    "ProductSession model selection is unavailable",
                ));
            }
        };
        if matches!(
            selection.as_ref().map(|selection| &selection.account_source),
            None | Some(
                winwincode_api::generated::ProviderAccountSource::SystemDefaultProviderAccountSource(_)
            )
        ) {
            let (route, credential_scope) = ModelSettingsService::new(self.storage)
                .resolve_with_catalog_scope(identity.target())
                .map_err(|error| map_model_settings_error(&error))?;
            let settings = ModelSettingsService::new(self.storage)
                .project(identity.target())
                .map_err(|error| map_model_settings_error(&error))?;
            let capability = ProviderCatalogService::new(self.storage)
                .resolve_model(&credential_scope, &route.provider_id, &route.model_id)
                .map_err(|error| map_provider_catalog_error(&error))?;
            return Ok(ResolvedRouteContext {
                route,
                credential_scope,
                settings,
                capability,
                provider_account_selected: false,
            });
        }
        let selection = selection.ok_or_else(ProviderGatewayError::invalid)?;
        let user_id = identity.user_id().ok_or_else(|| {
            ProviderGatewayError::new(
                ProviderGatewayErrorKind::IdentityDenied,
                "Provider account route requires a user identity",
            )
        })?;
        let period_id = message.sent_at.0.get(..7).filter(|value| {
            value.as_bytes().get(4) == Some(&b'-')
                && value
                    .bytes()
                    .enumerate()
                    .all(|(index, byte)| index == 4 || byte.is_ascii_digit())
        });
        let period_id = period_id.ok_or_else(ProviderGatewayError::invalid)?;
        let account = crate::provider_account::ProviderAccountRoutingService::new(self.storage)
            .select_for_exchange(
                user_id,
                repository_scope,
                &selection,
                &message.model_exchange_id,
                period_id,
                &message.sent_at,
            )
            .map_err(map_provider_account_route_error)?
            .ok_or_else(|| {
                ProviderGatewayError::new(
                    ProviderGatewayErrorKind::RouteUnavailable,
                    "Provider account route was not selected",
                )
            })?;
        let route = ModelRoute {
            credential_reference_id: account.credential_reference_id.clone(),
            model_id: selection.model_id.clone(),
            provider_id: selection.provider_id.clone(),
        };
        let mut settings = ModelSettingsService::new(self.storage)
            .project(identity.target())
            .map_err(|error| map_model_settings_error(&error))?;
        settings.selection = Some(crate::ModelSelection {
            provider_id: selection.provider_id.clone(),
            model_id: selection.model_id.clone(),
        });
        settings.default_model_route = Some(route.clone());
        let mut capability = ProviderCatalogService::new(self.storage)
            .resolve_model(
                &account.credential_scope,
                &selection.provider_id,
                &selection.model_id,
            )
            .map_err(|error| map_provider_catalog_error(&error))?;
        capability.credential_reference_id = account.credential_reference_id;
        Ok(ResolvedRouteContext {
            route,
            credential_scope: account.credential_scope,
            settings,
            capability,
            provider_account_selected: true,
        })
    }

    fn release_provider_account_route(
        &mut self,
        identity: &ProviderGatewayIdentity,
        model_exchange_id: &ModelExchangeId,
        used_tokens: u64,
        settlement: crate::provider_account::ProviderAccountExchangeSettlement,
    ) -> Result<(), ProviderGatewayError> {
        let ModelSettingsTarget::ProductSession {
            repository_scope, ..
        } = identity.target()
        else {
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::IdentityDenied,
                "Provider Gateway target is not a ProductSession",
            ));
        };
        crate::provider_account::ProviderAccountRoutingService::new(self.storage)
            .settle_exchange(repository_scope, model_exchange_id, used_tokens, settlement)
            .map_err(map_provider_account_route_error)
    }

    fn invoke_provider(
        &mut self,
        message: &ModelOpenMessage,
        payload: &[u8],
        resolved: &ResolvedProviderOpen,
        adapter_request_id: &str,
    ) -> Result<(ProviderAdapterOpenReceipt, CredentialLeakGate), ProviderGatewayError> {
        let credential = match self.secret_store.resolve(&resolved.reference) {
            Ok(credential) => credential,
            Err(error) => {
                self.release_failed_open(&resolved.admission, message)?;
                return Err(map_secret_store_error(&error));
            }
        };
        let mut leak_gate = CredentialLeakGate::new();
        leak_gate.track_secret(&credential);
        let payload_inspection = if is_json_content_type(&message.request.content_type) {
            leak_gate.inspect_json_bytes(CredentialOutputBoundary::Serialization, payload)
        } else {
            leak_gate.inspect_bytes(CredentialOutputBoundary::Serialization, payload)
        };
        if let Err(error) = payload_inspection {
            drop(credential);
            self.release_failed_open(&resolved.admission, message)?;
            return Err(error.into());
        }
        let invocation = ProviderAdapterInvocation {
            model_exchange_id: &message.model_exchange_id,
            request_id: &message.request_id,
            adapter_request_id,
            model_id: &resolved.route.model_id,
            content_type: &message.request.content_type,
            payload,
        };
        let adapter_result = self
            .adapter(&resolved.route.provider_id)?
            .open(&invocation, &credential)
            .map_err(|error| map_adapter_error(&error));
        let adapter_receipt = match adapter_result {
            Ok(receipt) => receipt,
            Err(error) => {
                drop(credential);
                self.release_failed_open(&resolved.admission, message)?;
                return Err(error);
            }
        };
        let receipt_inspection = leak_gate.inspect_bytes(
            CredentialOutputBoundary::Serialization,
            adapter_receipt.adapter_request_id.as_bytes(),
        );
        drop(credential);
        if let Err(error) = receipt_inspection {
            let provider_cleanup = self.cleanup_rejected_adapter_open(
                &resolved.route.provider_id,
                &message.model_exchange_id,
                adapter_request_id,
            );
            let admission_cleanup = self.release_failed_open(&resolved.admission, message);
            provider_cleanup?;
            admission_cleanup?;
            return Err(error.into());
        }
        if adapter_receipt.adapter_request_id != adapter_request_id {
            let provider_cleanup = self.cleanup_rejected_adapter_open(
                &resolved.route.provider_id,
                &message.model_exchange_id,
                adapter_request_id,
            );
            let admission_cleanup = self.release_failed_open(&resolved.admission, message);
            provider_cleanup?;
            admission_cleanup?;
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::AdapterProtocol,
                "Provider adapter changed its precommitted request identity",
            ));
        }
        Ok((adapter_receipt, leak_gate))
    }

    /// Pauses or resumes reads from the selected Provider transport.
    ///
    /// Identical requests replay without invoking the adapter again. A
    /// terminal exchange cannot resume Provider reads.
    ///
    /// # Errors
    ///
    /// Rejects unknown or terminal exchanges and maps adapter failures to a
    /// stable secret-free Gateway category.
    pub fn set_provider_read_paused(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        paused: bool,
    ) -> Result<ProviderStreamControlReceipt, ProviderGatewayError> {
        validate_prefixed_id(&model_exchange_id.0, "mdl_", 200)?;
        let action = if paused {
            ProviderStreamControlAction::Pause
        } else {
            ProviderStreamControlAction::Resume
        };
        let record = self.exchange(model_exchange_id)?;
        if record.terminal.is_some() || record.terminating.is_some() {
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::TerminalConflict,
                "Model exchange is terminating",
            ));
        }
        if record.provider_read.synchronized && record.provider_read.paused == paused {
            return Ok(ProviderStreamControlReceipt {
                action,
                replayed: true,
            });
        }

        self.call_adapter_control(model_exchange_id, action)?;
        let record = self.exchange_mut(model_exchange_id)?;
        record.provider_read.paused = paused;
        record.provider_read.synchronized = true;
        Ok(ProviderStreamControlReceipt {
            action,
            replayed: false,
        })
    }

    /// Validates an authoritative Worker cancellation acknowledgement and
    /// terminates the exact exchange.
    ///
    /// # Errors
    ///
    /// Rejects malformed cancellation signals and stale Worker authority.
    pub fn cancel_from_worker(
        &mut self,
        acknowledgement: &ModelAckMessage,
    ) -> Result<ProviderGatewayTerminalReceipt, ProviderGatewayError> {
        let record = self.exchange(&acknowledgement.model_exchange_id)?;
        validate_worker_cancellation(record, acknowledgement)?;
        self.apply_terminal(
            &acknowledgement.model_exchange_id,
            ProviderGatewayTerminal::Cancelled,
            &acknowledgement.sent_at,
        )
    }

    /// Validates a normal Worker stream acknowledgement against the exact
    /// durable lease and session authority without applying Provider control.
    ///
    /// # Errors
    ///
    /// Rejects expired, foreign, malformed, replay-request, or error acknowledgements.
    pub fn validate_worker_acknowledgement(
        &self,
        acknowledgement: &ModelAckMessage,
    ) -> Result<(), ProviderGatewayError> {
        let record = self.exchange(&acknowledgement.model_exchange_id)?;
        validate_worker_ack_authority(record, acknowledgement)?;
        if acknowledgement.error.is_some()
            || acknowledgement.replay_from_sequence.is_some()
            || !matches!(
                acknowledgement.status,
                LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
            )
        {
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::IdentityDenied,
                "Worker acknowledgement authority was denied",
            ));
        }
        Ok(())
    }

    /// Validates Worker cancellation and durably checkpoints terminal effects.
    ///
    /// # Errors
    ///
    /// Rejects malformed or stale Worker authority and any progress/Gateway failure.
    pub fn cancel_from_worker_with_progress(
        &mut self,
        acknowledgement: &ModelAckMessage,
        progress: &dyn ProviderGatewayTerminalProgressPort,
    ) -> Result<ProviderGatewayTerminalReceipt, ProviderGatewayError> {
        let record = self.exchange(&acknowledgement.model_exchange_id)?;
        validate_worker_cancellation(record, acknowledgement)?;
        self.apply_terminal_with_progress(
            &acknowledgement.model_exchange_id,
            ProviderGatewayTerminal::Cancelled,
            progress,
            &acknowledgement.sent_at,
        )
    }

    /// Applies one trusted terminal command to an opened exchange at most once.
    ///
    /// A duplicate identical terminal is replayed without calling settlement
    /// again. A failed settlement remains retryable.
    ///
    /// # Errors
    ///
    /// Rejects unknown exchanges and conflicting terminal outcomes.
    pub fn apply_terminal(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        command: ProviderGatewayTerminal,
        observed_at: &Instant,
    ) -> Result<ProviderGatewayTerminalReceipt, ProviderGatewayError> {
        self.settle_terminal(model_exchange_id, command, None, observed_at)
    }

    /// Releases a reservation after an opening tombstone is recovered without
    /// resolving a Credential or invoking a Provider adapter.
    ///
    /// # Errors
    ///
    /// Propagates exact durable admission identity or settlement failures.
    pub fn release_interrupted_open(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ModelReservationTerminalReceipt>, ProviderGatewayError> {
        self.admission
            .release_if_reserved(
                authority,
                original_request_id,
                model_exchange_id,
                ModelReservationReleaseReason::ProviderFailed,
            )
            .map_err(|error| map_admission_terminal_error(&error))
    }

    /// Fences a precommitted but incompletely persisted Provider open.
    ///
    /// The adapter idempotency identity is written before the original open,
    /// so exact Cancel and Release are safe whether the process stopped before
    /// or after the upstream accepted it. No Credential is resolved here.
    ///
    /// # Errors
    ///
    /// Rejects changed Provider identity and propagates adapter or exact
    /// admission cleanup failure.
    pub fn cleanup_interrupted_open(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        provider_id: &str,
        adapter_request_id: &str,
    ) -> Result<Option<ModelReservationTerminalReceipt>, ProviderGatewayError> {
        if authority.route().provider_id != provider_id {
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::ExchangeConflict,
                "Durable interrupted-open Provider identity changed",
            ));
        }
        validate_token(adapter_request_id, 200)?;
        self.cleanup_rejected_adapter_open(provider_id, model_exchange_id, adapter_request_id)?;
        self.release_interrupted_open(authority, original_request_id, model_exchange_id)
    }

    /// Applies a terminal command while durably fencing every external step.
    ///
    /// # Errors
    ///
    /// Rejects unknown/conflicting exchanges and fails closed when a durable
    /// checkpoint or external terminal operation fails.
    pub fn apply_terminal_with_progress(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        command: ProviderGatewayTerminal,
        progress: &dyn ProviderGatewayTerminalProgressPort,
        observed_at: &Instant,
    ) -> Result<ProviderGatewayTerminalReceipt, ProviderGatewayError> {
        self.settle_terminal(
            model_exchange_id,
            command,
            Some((progress, observed_at)),
            observed_at,
        )
    }

    fn settle_terminal(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        command: ProviderGatewayTerminal,
        progress: Option<(&dyn ProviderGatewayTerminalProgressPort, &Instant)>,
        observed_at: &Instant,
    ) -> Result<ProviderGatewayTerminalReceipt, ProviderGatewayError> {
        let (mut durable_progress, replay) =
            self.prepare_terminal(model_exchange_id, command, progress)?;
        if let Some(replay) = replay {
            return Ok(replay);
        }
        let outcome = command.outcome();
        self.release_provider_terminal(
            model_exchange_id,
            outcome,
            command,
            progress,
            &mut durable_progress,
        )?;
        record_terminal_progress_if_needed(
            &mut durable_progress,
            progress,
            model_exchange_id,
            command,
            ProviderGatewayTerminalProgressStage::AdmissionStarted,
            None,
            None,
        )?;
        let admission = durable_progress
            .as_ref()
            .and_then(|snapshot| snapshot.admission.clone())
            .map_or_else(
                || self.settle_admission_terminal(model_exchange_id, command),
                Ok,
            )?;
        record_terminal_progress_if_needed(
            &mut durable_progress,
            progress,
            model_exchange_id,
            command,
            ProviderGatewayTerminalProgressStage::AdmissionSettled,
            Some(&admission),
            None,
        )?;
        let settlement =
            self.gateway_settlement(model_exchange_id, command, &admission, observed_at)?;
        CredentialLeakGate::default()
            .inspect_serializable(CredentialOutputBoundary::Event, &settlement)?;
        record_terminal_progress_if_needed(
            &mut durable_progress,
            progress,
            model_exchange_id,
            command,
            ProviderGatewayTerminalProgressStage::SettlementStarted,
            Some(&admission),
            None,
        )?;
        self.settlement.settle(&settlement).map_err(|_error| {
            ProviderGatewayError::new(
                ProviderGatewayErrorKind::SettlementUnavailable,
                "Provider Gateway settlement operation failed",
            )
        })?;
        self.settle_provider_account_usage(model_exchange_id, command)?;
        let receipt = ProviderGatewayTerminalReceipt {
            model_exchange_id: model_exchange_id.clone(),
            outcome,
            admission,
            settled_at: observed_at.clone(),
            idempotent_replay: false,
        };
        record_terminal_progress_if_needed(
            &mut durable_progress,
            progress,
            model_exchange_id,
            command,
            ProviderGatewayTerminalProgressStage::SettlementSettled,
            Some(&receipt.admission),
            Some(&receipt),
        )?;
        let record = self.exchange_mut(model_exchange_id)?;
        record.terminal = Some((command, receipt.clone()));
        record.terminating = None;
        Ok(receipt)
    }

    fn settle_provider_account_usage(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        command: ProviderGatewayTerminal,
    ) -> Result<(), ProviderGatewayError> {
        let repository_scope = match self
            .exchange(model_exchange_id)?
            .admission_authority
            .target()
        {
            ModelSettingsTarget::ProductSession {
                repository_scope, ..
            } => repository_scope.clone(),
            ModelSettingsTarget::Organization { .. }
            | ModelSettingsTarget::Project { .. }
            | ModelSettingsTarget::Repository { .. } => {
                return Err(ProviderGatewayError::new(
                    ProviderGatewayErrorKind::IdentityDenied,
                    "Provider account exchange target is invalid",
                ));
            }
        };
        let used_tokens = command.charge().map_or(0, |charge| {
            charge
                .usage
                .input_tokens
                .saturating_add(charge.usage.output_tokens)
        });
        crate::provider_account::ProviderAccountRoutingService::new(self.storage)
            .settle_exchange(
                &repository_scope,
                model_exchange_id,
                used_tokens,
                crate::provider_account::ProviderAccountExchangeSettlement::Final,
            )
            .map_err(map_provider_account_route_error)
    }

    fn prepare_terminal(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        command: ProviderGatewayTerminal,
        progress: Option<(&dyn ProviderGatewayTerminalProgressPort, &Instant)>,
    ) -> Result<
        (
            Option<ProviderGatewayTerminalProgress>,
            Option<ProviderGatewayTerminalReceipt>,
        ),
        ProviderGatewayError,
    > {
        if let Some(replay) = self.begin_terminal(model_exchange_id, command)? {
            return Ok((None, Some(replay)));
        }
        let mut durable = load_terminal_progress(progress, model_exchange_id, command)?;
        if let Some(mut terminal) = durable
            .as_ref()
            .and_then(|snapshot| snapshot.terminal.clone())
        {
            if terminal.model_exchange_id != *model_exchange_id
                || terminal.outcome != command.outcome()
            {
                return Err(ProviderGatewayError::new(
                    ProviderGatewayErrorKind::TerminalConflict,
                    "Durable Provider terminal receipt conflicts with the command",
                ));
            }
            terminal.idempotent_replay = true;
            let record = self.exchange_mut(model_exchange_id)?;
            record.terminal = Some((command, terminal.clone()));
            record.terminating = None;
            return Ok((durable, Some(terminal)));
        }
        self.restore_terminal_flags(model_exchange_id, durable.as_ref())?;
        record_terminal_progress_if_needed(
            &mut durable,
            progress,
            model_exchange_id,
            command,
            ProviderGatewayTerminalProgressStage::Prepared,
            None,
            None,
        )?;
        Ok((durable, None))
    }

    fn restore_terminal_flags(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        progress: Option<&ProviderGatewayTerminalProgress>,
    ) -> Result<(), ProviderGatewayError> {
        let Some(progress) = progress else {
            return Ok(());
        };
        let record = self.exchange_mut(model_exchange_id)?;
        if progress.stage >= ProviderGatewayTerminalProgressStage::Cancelled {
            record.provider_terminal.cancelled = true;
        }
        if progress.stage >= ProviderGatewayTerminalProgressStage::Released {
            record.provider_terminal.released = true;
            record.provider_read.paused = false;
            record.provider_read.synchronized = true;
        }
        Ok(())
    }

    fn begin_terminal(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        command: ProviderGatewayTerminal,
    ) -> Result<Option<ProviderGatewayTerminalReceipt>, ProviderGatewayError> {
        validate_prefixed_id(&model_exchange_id.0, "mdl_", 200)?;
        let record = self.exchange(model_exchange_id)?;
        if let Some((terminal_command, terminal_receipt)) = &record.terminal {
            if terminal_command != &command {
                return Err(ProviderGatewayError::new(
                    ProviderGatewayErrorKind::TerminalConflict,
                    "Model exchange was already settled with another outcome",
                ));
            }
            let mut replay = terminal_receipt.clone();
            replay.idempotent_replay = true;
            return Ok(Some(replay));
        }
        if record.terminating.is_some_and(|pending| pending != command) {
            return Err(ProviderGatewayError::new(
                ProviderGatewayErrorKind::TerminalConflict,
                "Model exchange is terminating with another outcome",
            ));
        }
        self.exchange_mut(model_exchange_id)?.terminating = Some(command);
        Ok(None)
    }

    fn release_provider_terminal(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        outcome: ProviderGatewayTerminalOutcome,
        command: ProviderGatewayTerminal,
        progress: Option<(&dyn ProviderGatewayTerminalProgressPort, &Instant)>,
        durable_progress: &mut Option<ProviderGatewayTerminalProgress>,
    ) -> Result<(), ProviderGatewayError> {
        if outcome != ProviderGatewayTerminalOutcome::Succeeded
            && !self
                .exchange(model_exchange_id)?
                .provider_terminal
                .cancelled
        {
            record_terminal_progress_if_needed(
                durable_progress,
                progress,
                model_exchange_id,
                command,
                ProviderGatewayTerminalProgressStage::CancelStarted,
                None,
                None,
            )?;
            self.call_adapter_control(model_exchange_id, ProviderStreamControlAction::Cancel)?;
            self.exchange_mut(model_exchange_id)?
                .provider_terminal
                .cancelled = true;
            record_terminal_progress_if_needed(
                durable_progress,
                progress,
                model_exchange_id,
                command,
                ProviderGatewayTerminalProgressStage::Cancelled,
                None,
                None,
            )?;
        }
        if !self.exchange(model_exchange_id)?.provider_terminal.released {
            record_terminal_progress_if_needed(
                durable_progress,
                progress,
                model_exchange_id,
                command,
                ProviderGatewayTerminalProgressStage::ReleaseStarted,
                None,
                None,
            )?;
            self.call_adapter_control(model_exchange_id, ProviderStreamControlAction::Release)?;
            let record = self.exchange_mut(model_exchange_id)?;
            record.provider_terminal.released = true;
            record.provider_read.paused = false;
            record.provider_read.synchronized = true;
            record_terminal_progress_if_needed(
                durable_progress,
                progress,
                model_exchange_id,
                command,
                ProviderGatewayTerminalProgressStage::Released,
                None,
                None,
            )?;
        }
        Ok(())
    }

    fn settle_admission_terminal(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        command: ProviderGatewayTerminal,
    ) -> Result<ModelReservationTerminalReceipt, ProviderGatewayError> {
        let (authority, original_request_id) = {
            let record = self.exchange(model_exchange_id)?;
            (
                record.admission_authority.clone(),
                record.open_receipt.request_id.clone(),
            )
        };
        let result = if let Some(charge) = command.charge() {
            self.admission.complete(
                &authority,
                &original_request_id,
                model_exchange_id,
                charge.usage,
                charge.actual_cost_micros,
            )
        } else {
            let reason = match command {
                ProviderGatewayTerminal::Failed { .. } => {
                    ModelReservationReleaseReason::ProviderFailed
                }
                ProviderGatewayTerminal::Cancelled => ModelReservationReleaseReason::Cancelled,
                ProviderGatewayTerminal::Completed { .. } => {
                    return Err(ProviderGatewayError::invalid());
                }
            };
            self.admission
                .release(&authority, &original_request_id, model_exchange_id, reason)
        };
        result.map_err(|error| map_admission_terminal_error(&error))
    }

    fn gateway_settlement(
        &self,
        model_exchange_id: &ModelExchangeId,
        command: ProviderGatewayTerminal,
        admission: &ModelReservationTerminalReceipt,
        settled_at: &Instant,
    ) -> Result<ProviderGatewaySettlement, ProviderGatewayError> {
        let record = self.exchange(model_exchange_id)?;
        let failure = match command {
            ProviderGatewayTerminal::Failed { failure, .. } => Some(failure),
            ProviderGatewayTerminal::Cancelled => Some(ModelAttemptFailureFact {
                kind: ModelAttemptFailureKind::Cancelled,
                certainty: ModelExecutionCertainty::AcceptanceUnknown,
            }),
            ProviderGatewayTerminal::Completed { .. } => None,
        };
        let charge = command.charge().map(|charge| ModelAttemptCharge {
            provider_usage_id: provider_usage_identity(
                model_exchange_id,
                &record.open_receipt.route.provider_id,
                &record.open_receipt.route.model_id,
                &record.open_receipt.adapter_request_id,
            ),
            usage: charge.usage,
            cost_micros: charge.actual_cost_micros,
        });
        Ok(ProviderGatewaySettlement {
            model_exchange_id: model_exchange_id.clone(),
            request_id: record.open_receipt.request_id.clone(),
            provider_id: record.open_receipt.route.provider_id.clone(),
            model_id: record.open_receipt.route.model_id.clone(),
            adapter_request_id: record.open_receipt.adapter_request_id.clone(),
            settled_at: settled_at.clone(),
            outcome: command.outcome(),
            admission_terminal: admission.clone(),
            failure,
            charge,
        })
    }

    fn release_failed_open(
        &mut self,
        admitted: &crate::ProviderAdmissionOpenReceipt,
        message: &ModelOpenMessage,
    ) -> Result<(), ProviderGatewayError> {
        self.admission
            .release(
                &admitted.route_authority,
                &message.request_id,
                &message.model_exchange_id,
                ModelReservationReleaseReason::ProviderFailed,
            )
            .map(|_| ())
            .map_err(|error| map_admission_terminal_error(&error))
    }

    fn cleanup_rejected_adapter_open(
        &self,
        provider_id: &str,
        model_exchange_id: &ModelExchangeId,
        adapter_request_id: &str,
    ) -> Result<(), ProviderGatewayError> {
        let adapter = self.adapter(provider_id)?;
        let cancelled = adapter
            .control(
                model_exchange_id,
                adapter_request_id,
                ProviderStreamControlAction::Cancel,
            )
            .map_err(|error| map_adapter_error(&error));
        let released = adapter
            .control(
                model_exchange_id,
                adapter_request_id,
                ProviderStreamControlAction::Release,
            )
            .map_err(|error| map_adapter_error(&error));
        cancelled.and(released)
    }

    fn exchange(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<&ExchangeRecord, ProviderGatewayError> {
        self.exchanges.get(&model_exchange_id.0).ok_or_else(|| {
            ProviderGatewayError::new(
                ProviderGatewayErrorKind::ExchangeNotFound,
                "Model exchange is not open in this Gateway",
            )
        })
    }

    fn exchange_mut(
        &mut self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<&mut ExchangeRecord, ProviderGatewayError> {
        self.exchanges.get_mut(&model_exchange_id.0).ok_or_else(|| {
            ProviderGatewayError::new(
                ProviderGatewayErrorKind::ExchangeNotFound,
                "Model exchange is not open in this Gateway",
            )
        })
    }

    fn call_adapter_control(
        &self,
        model_exchange_id: &ModelExchangeId,
        action: ProviderStreamControlAction,
    ) -> Result<(), ProviderGatewayError> {
        let record = self.exchange(model_exchange_id)?;
        let adapter = self.adapter(&record.open_receipt.route.provider_id)?;
        adapter
            .control(
                model_exchange_id,
                &record.open_receipt.adapter_request_id,
                action,
            )
            .map_err(|error| map_adapter_error(&error))
    }

    fn adapter(&self, provider_id: &str) -> Result<&dyn ProviderAdapterPort, ProviderGatewayError> {
        self.adapters
            .get(provider_id)
            .map(Box::as_ref)
            .ok_or_else(|| {
                ProviderGatewayError::new(
                    ProviderGatewayErrorKind::AdapterNotRegistered,
                    "No adapter is registered for the configured Provider",
                )
            })
    }
}

fn validate_worker_cancellation(
    record: &ExchangeRecord,
    acknowledgement: &ModelAckMessage,
) -> Result<(), ProviderGatewayError> {
    validate_worker_ack_authority(record, acknowledgement)?;
    let cancellation = acknowledgement.error.as_ref();
    if acknowledgement.status != LeaseWriteStatus::RejectedConflict
        || acknowledgement.ack_sequence.0 < 0
        || acknowledgement.replay_from_sequence.is_some()
        || cancellation.is_none_or(|error| {
            error.code != ExecutionPortErrorCode::Cancelled
                || error.retryable
                || error.message != MODEL_CANCELLATION_MESSAGE
        })
    {
        return Err(ProviderGatewayError::new(
            ProviderGatewayErrorKind::IdentityDenied,
            "Worker cancellation authority was denied",
        ));
    }
    Ok(())
}

fn validate_worker_ack_authority(
    record: &ExchangeRecord,
    acknowledgement: &ModelAckMessage,
) -> Result<(), ProviderGatewayError> {
    if acknowledgement.schema_version != SchemaVersion::WinwincodeV1
        || acknowledgement.ack_sequence.0 < 0
        || acknowledgement.lease != record.lease
        || acknowledgement.sent_at.0 >= record.lease.expires_at.0
        || acknowledgement.worker_session_id != record.worker_session_id
        || acknowledgement.session_identity != record.session_identity
    {
        return Err(ProviderGatewayError::new(
            ProviderGatewayErrorKind::IdentityDenied,
            "Worker acknowledgement authority was denied",
        ));
    }
    validate_prefixed_id(&acknowledgement.message_id.0, "xmsg_", 200)
}

fn validate_open_envelope(message: &ModelOpenMessage) -> Result<(), ProviderGatewayError> {
    if message.worker_session_id != message.session_identity.worker_session_id {
        return Err(ProviderGatewayError::invalid());
    }
    validate_prefixed_id(&message.message_id.0, "xmsg_", 200)?;
    validate_prefixed_id(&message.model_exchange_id.0, "mdl_", 200)?;
    validate_prefixed_id(&message.request_id.0, "req_", 200)?;
    validate_prefixed_id(&message.session_identity.product_session_id.0, "psn_", 200)?;
    validate_prefixed_id(&message.worker_session_id.0, "wsn_", 200)?;
    validate_prefixed_id(&message.session_identity.codex_thread_id.0, "cdx_", 200)?;
    validate_token(&message.route.route, 120)?;
    validate_token(&message.route.capability, 120)?;
    Ok(())
}

fn validate_durable_exchange(
    exchange: &ProviderGatewayDurableExchange,
) -> Result<(), ProviderGatewayError> {
    exchange
        .route_authority
        .validate_fingerprint()
        .map_err(|_| ProviderGatewayError::invalid())?;
    validate_prefixed_id(&exchange.open_receipt.model_exchange_id.0, "mdl_", 200)?;
    validate_prefixed_id(&exchange.open_receipt.request_id.0, "req_", 200)?;
    validate_model_route(&exchange.open_receipt.route)?;
    validate_token(&exchange.open_receipt.adapter_request_id, 200)?;
    validate_prefixed_id(&exchange.worker_session_id.0, "wsn_", 200)?;
    validate_prefixed_id(&exchange.session_identity.product_session_id.0, "psn_", 200)?;
    validate_prefixed_id(&exchange.session_identity.codex_thread_id.0, "cdx_", 200)?;
    let ModelSettingsTarget::ProductSession {
        product_session_id, ..
    } = exchange.route_authority.target()
    else {
        return Err(ProviderGatewayError::invalid());
    };
    if exchange.route_authority.route() != &exchange.open_receipt.route
        || exchange.worker_session_id != exchange.session_identity.worker_session_id
        || product_session_id != &exchange.session_identity.product_session_id
        || exchange.lease.attempt <= 0
        || exchange.lease.issued_at.0.is_empty()
        || exchange.lease.expires_at.0.is_empty()
    {
        return Err(ProviderGatewayError::invalid());
    }
    Ok(())
}

fn lower_hex(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(ALPHABET[usize::from(byte >> 4)]));
        output.push(char::from(ALPHABET[usize::from(byte & 0x0f)]));
    }
    output
}

fn record_terminal_progress(
    progress: Option<(&dyn ProviderGatewayTerminalProgressPort, &Instant)>,
    model_exchange_id: &ModelExchangeId,
    command: ProviderGatewayTerminal,
    stage: ProviderGatewayTerminalProgressStage,
    admission: Option<&ModelReservationTerminalReceipt>,
    terminal: Option<&ProviderGatewayTerminalReceipt>,
) -> Result<(), ProviderGatewayError> {
    if let Some((progress, observed_at)) = progress {
        progress.record(
            model_exchange_id,
            command,
            stage,
            admission,
            terminal,
            observed_at,
        )?;
    }
    Ok(())
}

fn load_terminal_progress(
    progress: Option<(&dyn ProviderGatewayTerminalProgressPort, &Instant)>,
    model_exchange_id: &ModelExchangeId,
    command: ProviderGatewayTerminal,
) -> Result<Option<ProviderGatewayTerminalProgress>, ProviderGatewayError> {
    let Some((progress, observed_at)) = progress else {
        return Ok(None);
    };
    let snapshot = progress.load(model_exchange_id)?;
    if let Some(snapshot) = &snapshot {
        progress.record(
            model_exchange_id,
            command,
            snapshot.stage,
            snapshot.admission.as_ref(),
            snapshot.terminal.as_ref(),
            observed_at,
        )?;
    }
    Ok(snapshot)
}

fn record_terminal_progress_if_needed(
    durable: &mut Option<ProviderGatewayTerminalProgress>,
    progress: Option<(&dyn ProviderGatewayTerminalProgressPort, &Instant)>,
    model_exchange_id: &ModelExchangeId,
    command: ProviderGatewayTerminal,
    stage: ProviderGatewayTerminalProgressStage,
    admission: Option<&ModelReservationTerminalReceipt>,
    terminal: Option<&ProviderGatewayTerminalReceipt>,
) -> Result<(), ProviderGatewayError> {
    if durable
        .as_ref()
        .is_some_and(|snapshot| snapshot.stage >= stage)
    {
        return Ok(());
    }
    record_terminal_progress(
        progress,
        model_exchange_id,
        command,
        stage,
        admission,
        terminal,
    )?;
    if progress.is_some() {
        *durable = Some(ProviderGatewayTerminalProgress {
            stage,
            admission: admission.cloned(),
            terminal: terminal.cloned(),
        });
    }
    Ok(())
}

fn parse_sha256(value: &Sha256Digest) -> Result<[u8; 32], ProviderGatewayError> {
    let Some(hex) = value.0.strip_prefix("sha256:") else {
        return Err(ProviderGatewayError::invalid());
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ProviderGatewayError::invalid());
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
    }
    Ok(digest)
}

fn hex_value(byte: u8) -> Result<u8, ProviderGatewayError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(ProviderGatewayError::invalid()),
    }
}

fn validate_identity(
    message: &ModelOpenMessage,
    identity: &ProviderGatewayIdentity,
) -> Result<(), ProviderGatewayError> {
    let ModelSettingsTarget::ProductSession {
        product_session_id, ..
    } = identity.target()
    else {
        return Err(ProviderGatewayError::new(
            ProviderGatewayErrorKind::IdentityDenied,
            "Provider Gateway identity is not a ProductSession target",
        ));
    };
    if product_session_id != &message.session_identity.product_session_id {
        return Err(ProviderGatewayError::new(
            ProviderGatewayErrorKind::IdentityDenied,
            "Provider Gateway identity does not match the model session",
        ));
    }
    Ok(())
}

fn map_identity_error(error: &ProviderGatewayIdentityError) -> ProviderGatewayError {
    match error.kind() {
        ProviderGatewayIdentityErrorKind::Denied => ProviderGatewayError::new(
            ProviderGatewayErrorKind::IdentityDenied,
            "Provider Gateway identity was denied",
        ),
        ProviderGatewayIdentityErrorKind::Unavailable => ProviderGatewayError::new(
            ProviderGatewayErrorKind::IdentityUnavailable,
            "Provider Gateway identity service is unavailable",
        ),
    }
}

fn map_model_settings_error(error: &ModelSettingsError) -> ProviderGatewayError {
    let (kind, message) = match error.kind() {
        ModelSettingsErrorKind::NoConfiguredRoute => (
            ProviderGatewayErrorKind::RouteUnavailable,
            "No model route is configured for this session",
        ),
        ModelSettingsErrorKind::ProviderNotFound => (
            ProviderGatewayErrorKind::ProviderNotFound,
            "Configured Provider is not present",
        ),
        ModelSettingsErrorKind::ProviderDisabled => (
            ProviderGatewayErrorKind::ProviderDisabled,
            "Configured Provider is disabled",
        ),
        ModelSettingsErrorKind::ModelNotFound => (
            ProviderGatewayErrorKind::ModelNotFound,
            "Configured model is not present",
        ),
        ModelSettingsErrorKind::ModelDisabled => (
            ProviderGatewayErrorKind::ModelDisabled,
            "Configured model is disabled",
        ),
        ModelSettingsErrorKind::CredentialLeak => (
            ProviderGatewayErrorKind::CredentialLeak,
            "Resolved model route was rejected by the Credential leak gate",
        ),
        ModelSettingsErrorKind::Storage => (
            ProviderGatewayErrorKind::Storage,
            "Provider Gateway storage operation failed",
        ),
        ModelSettingsErrorKind::ScopeDenied => (
            ProviderGatewayErrorKind::IdentityDenied,
            "Model settings target scope was denied",
        ),
        ModelSettingsErrorKind::InvalidRequest
        | ModelSettingsErrorKind::AlreadyMigrated
        | ModelSettingsErrorKind::RevisionConflict
        | ModelSettingsErrorKind::RequestConflict => (
            ProviderGatewayErrorKind::InvalidRequest,
            "Configured model route state is invalid",
        ),
    };
    ProviderGatewayError::new(kind, message)
}

fn map_provider_catalog_error(error: &ProviderCatalogError) -> ProviderGatewayError {
    let (kind, message) = match error.kind() {
        ProviderCatalogErrorKind::ProviderNotFound => (
            ProviderGatewayErrorKind::ProviderNotFound,
            "Configured Provider is not present",
        ),
        ProviderCatalogErrorKind::ProviderDisabled | ProviderCatalogErrorKind::AlreadyDisabled => (
            ProviderGatewayErrorKind::ProviderDisabled,
            "Configured Provider is disabled",
        ),
        ProviderCatalogErrorKind::ModelNotFound => (
            ProviderGatewayErrorKind::ModelNotFound,
            "Configured model is not present",
        ),
        ProviderCatalogErrorKind::ModelDisabled => (
            ProviderGatewayErrorKind::ModelDisabled,
            "Configured model is disabled",
        ),
        ProviderCatalogErrorKind::ScopeDenied => (
            ProviderGatewayErrorKind::IdentityDenied,
            "Provider catalog scope was denied",
        ),
        ProviderCatalogErrorKind::CredentialLeak => (
            ProviderGatewayErrorKind::CredentialLeak,
            "Provider catalog resolution failed its Credential leak gate",
        ),
        ProviderCatalogErrorKind::Storage => (
            ProviderGatewayErrorKind::Storage,
            "Provider Gateway storage operation failed",
        ),
        ProviderCatalogErrorKind::InvalidRequest
        | ProviderCatalogErrorKind::VersionConflict
        | ProviderCatalogErrorKind::RequestConflict => (
            ProviderGatewayErrorKind::InvalidRequest,
            "Provider catalog state is invalid",
        ),
    };
    ProviderGatewayError::new(kind, message)
}

fn map_admission_reserve_error(error: &ProviderAdmissionError) -> ProviderGatewayError {
    let (kind, message) = match error.kind() {
        ProviderAdmissionErrorKind::InvalidRequest => (
            ProviderGatewayErrorKind::InvalidRequest,
            "Provider admission request is invalid",
        ),
        ProviderAdmissionErrorKind::IdentityMismatch => (
            ProviderGatewayErrorKind::RouteMismatch,
            "Provider admission authority does not match the resolved route",
        ),
        ProviderAdmissionErrorKind::Conflict => (
            ProviderGatewayErrorKind::ExchangeConflict,
            "Model exchange conflicts with a durable admission reservation",
        ),
        ProviderAdmissionErrorKind::AuthorityUnavailable
        | ProviderAdmissionErrorKind::ClockUnavailable => (
            ProviderGatewayErrorKind::AdmissionUnavailable,
            "Provider admission authority is unavailable",
        ),
        ProviderAdmissionErrorKind::Storage => (
            ProviderGatewayErrorKind::Storage,
            "Provider admission storage operation failed",
        ),
    };
    ProviderGatewayError::new(kind, message)
}

fn map_admission_terminal_error(error: &ProviderAdmissionError) -> ProviderGatewayError {
    let (kind, message) = match error.kind() {
        ProviderAdmissionErrorKind::Conflict | ProviderAdmissionErrorKind::IdentityMismatch => (
            ProviderGatewayErrorKind::TerminalConflict,
            "Provider admission terminal conflicts with durable authority",
        ),
        ProviderAdmissionErrorKind::InvalidRequest => (
            ProviderGatewayErrorKind::InvalidRequest,
            "Provider admission terminal is invalid",
        ),
        ProviderAdmissionErrorKind::AuthorityUnavailable
        | ProviderAdmissionErrorKind::ClockUnavailable => (
            ProviderGatewayErrorKind::AdmissionUnavailable,
            "Provider admission terminal authority is unavailable",
        ),
        ProviderAdmissionErrorKind::Storage => (
            ProviderGatewayErrorKind::Storage,
            "Provider admission storage operation failed",
        ),
    };
    ProviderGatewayError::new(kind, message)
}

fn map_credential_reference_error(error: &CredentialReferenceError) -> ProviderGatewayError {
    match error.kind() {
        CredentialReferenceErrorKind::ScopeDenied => ProviderGatewayError::new(
            ProviderGatewayErrorKind::CredentialScopeMismatch,
            "Credential reference belongs to another scope",
        ),
        CredentialReferenceErrorKind::NotFound
        | CredentialReferenceErrorKind::Revoked
        | CredentialReferenceErrorKind::WrongState => {
            ProviderGatewayError::credential_unavailable()
        }
        CredentialReferenceErrorKind::CredentialLeak => ProviderGatewayError::new(
            ProviderGatewayErrorKind::CredentialLeak,
            "Credential reference was rejected by the Credential leak gate",
        ),
        CredentialReferenceErrorKind::Storage => ProviderGatewayError::new(
            ProviderGatewayErrorKind::Storage,
            "Provider Gateway storage operation failed",
        ),
        CredentialReferenceErrorKind::InvalidRequest
        | CredentialReferenceErrorKind::CursorInvalid
        | CredentialReferenceErrorKind::RevisionConflict
        | CredentialReferenceErrorKind::RequestConflict => ProviderGatewayError::invalid(),
    }
}

fn map_secret_store_error(_error: &SecretStoreError) -> ProviderGatewayError {
    ProviderGatewayError::credential_unavailable()
}

fn map_provider_account_route_error(
    error: crate::provider_account::ProviderAccountError,
) -> ProviderGatewayError {
    use crate::provider_account::ProviderAccountErrorKind;
    match error.kind() {
        ProviderAccountErrorKind::PermissionDenied => ProviderGatewayError::new(
            ProviderGatewayErrorKind::IdentityDenied,
            "Provider account route is not authorized",
        ),
        ProviderAccountErrorKind::NotFound
        | ProviderAccountErrorKind::WrongState
        | ProviderAccountErrorKind::RevisionConflict => ProviderGatewayError::new(
            ProviderGatewayErrorKind::RouteUnavailable,
            "Provider account route is unavailable",
        ),
        ProviderAccountErrorKind::InvalidRequest | ProviderAccountErrorKind::RequestConflict => {
            ProviderGatewayError::new(
                ProviderGatewayErrorKind::RouteMismatch,
                "Provider account route conflicts with the model exchange",
            )
        }
        ProviderAccountErrorKind::ProviderUnavailable
        | ProviderAccountErrorKind::SecretStore
        | ProviderAccountErrorKind::Storage => ProviderGatewayError::new(
            ProviderGatewayErrorKind::RouteUnavailable,
            "Provider account route authority is unavailable",
        ),
    }
}

fn map_adapter_error(error: &ProviderAdapterError) -> ProviderGatewayError {
    match error.kind() {
        ProviderAdapterErrorKind::Rejected => ProviderGatewayError::new(
            ProviderGatewayErrorKind::AdapterRejected,
            "Provider rejected the request",
        ),
        ProviderAdapterErrorKind::RateLimited => ProviderGatewayError::new(
            ProviderGatewayErrorKind::AdapterRateLimited,
            "Provider rate limit rejected the request",
        ),
        ProviderAdapterErrorKind::Unavailable => ProviderGatewayError::new(
            ProviderGatewayErrorKind::AdapterUnavailable,
            "Provider adapter is unavailable",
        ),
        ProviderAdapterErrorKind::Protocol => ProviderGatewayError::new(
            ProviderGatewayErrorKind::AdapterProtocol,
            "Provider adapter response is invalid",
        ),
    }
}

fn decode_payload(payload: &EncodedPayload) -> Result<Vec<u8>, ProviderGatewayError> {
    validate_content_type(&payload.content_type)?;
    if payload.data_base64.len() > MAX_PROVIDER_PAYLOAD_BYTES.saturating_mul(2) {
        return Err(ProviderGatewayError::invalid());
    }
    let bytes = STANDARD
        .decode(&payload.data_base64)
        .map_err(|_| ProviderGatewayError::invalid())?;
    if bytes.len() > MAX_PROVIDER_PAYLOAD_BYTES || STANDARD.encode(&bytes) != payload.data_base64 {
        return Err(ProviderGatewayError::invalid());
    }
    let expected = format!("sha256:{:x}", Sha256::digest(&bytes));
    if payload.payload_digest.0 != expected {
        return Err(ProviderGatewayError::invalid());
    }
    Ok(bytes)
}

fn gateway_open_digest(
    message: &ModelOpenMessage,
    route: &ModelRoute,
    target: &ModelSettingsTarget,
    adapter_request_id: &str,
) -> Result<[u8; 32], ProviderGatewayError> {
    let payload = serde_json::to_vec(&(message, route, target, adapter_request_id))
        .map_err(|_| ProviderGatewayError::invalid())?;
    Ok(Sha256::digest(payload).into())
}

fn provider_usage_identity(
    model_exchange_id: &ModelExchangeId,
    provider_id: &str,
    model_id: &str,
    adapter_request_id: &str,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.provider-usage.v1\0");
    for value in [
        model_exchange_id.0.as_str(),
        provider_id,
        model_id,
        adapter_request_id,
    ] {
        digest.update(
            u64::try_from(value.len())
                .expect("validated Provider identity length fits u64")
                .to_be_bytes(),
        );
        digest.update(value.as_bytes());
    }
    format!("provider-usage:sha256:{:x}", digest.finalize())
}

fn validate_model_route(route: &ModelRoute) -> Result<(), ProviderGatewayError> {
    validate_token(&route.provider_id, 128)?;
    validate_token(&route.model_id, 200)?;
    validate_prefixed_id(&route.credential_reference_id.0, "crd_", 200)
}

fn validate_content_type(value: &str) -> Result<(), ProviderGatewayError> {
    if value.is_empty()
        || value.len() > 200
        || value.chars().any(char::is_control)
        || !value.contains('/')
    {
        return Err(ProviderGatewayError::invalid());
    }
    Ok(())
}

fn is_json_content_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|mime| mime.trim().ends_with("/json") || mime.trim().ends_with("+json"))
}

fn validate_token(value: &str, max_len: usize) -> Result<(), ProviderGatewayError> {
    if value.is_empty() || value.len() > max_len || value.chars().any(char::is_control) {
        return Err(ProviderGatewayError::invalid());
    }
    Ok(())
}

fn validate_prefixed_id(
    value: &str,
    prefix: &str,
    max_len: usize,
) -> Result<(), ProviderGatewayError> {
    validate_token(value, max_len)?;
    if !value.starts_with(prefix) || value.len() == prefix.len() {
        return Err(ProviderGatewayError::invalid());
    }
    Ok(())
}
