// SPDX-License-Identifier: Apache-2.0

//! Durable, route-isolated model policy admission.
//!
//! The module runs before a Provider adapter is invoked. It binds every
//! reservation to one trusted Gateway scope, resolved route, catalog version,
//! Credential rotation, and frozen policy snapshot. The durable ledger owns
//! only operational reservations; final organization billing remains outside
//! this module and consumes the settled usage facts.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{ModelRoute, Scope};
use winwincode_domain::{ModelExchangeId, RequestId, Sha256Digest};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity,
    ReceiptScopeKey, StateCommit, StorageError, StorageErrorKind, StoredState,
};

use crate::{
    CredentialReferenceResolution, ModelRequestAdmission, ModelRequestRouteKey,
    ModelSettingsProjection, ModelSettingsTarget, ProviderGatewayIdentity, ProviderTokenUsage,
    ResolvedModelCapability,
};

const STATE_SCHEMA: &str = "winwincode.model-admission.v1";
const EVENT_SCHEMA: &str = "winwincode.model-admission-event.v1";
const STREAM_PREFIX: &str = "model-admission:";
const EVENT_TOPIC: &str = "model.admission.changed.v1";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_COMMIT_RETRIES: usize = 64;
const MAX_DURABLE_ROUTE_AUTHORITY_BYTES: usize = 64 * 1024;

/// One fixed UTC minute supplied by an authoritative clock.
pub trait ModelAdmissionClock: Send + Sync {
    /// Returns the current Unix minute.
    ///
    /// # Errors
    ///
    /// Returns a stable dependency failure when the clock is unavailable.
    fn unix_minute(&self) -> Result<u64, ModelAdmissionClockError>;
}

/// Stable authoritative-clock failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelAdmissionClockError;

impl fmt::Display for ModelAdmissionClockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("model admission clock is unavailable")
    }
}

impl std::error::Error for ModelAdmissionClockError {}

/// Whether one policy authority permits the already resolved route.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRoutePolicyDecision {
    /// The authority permits the exact route.
    Allow,
    /// The authority denies the exact route.
    Deny,
}

/// Operational limits frozen before Provider admission.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelAdmissionLimits {
    /// Accepted requests in one fixed UTC minute.
    pub requests_per_minute: u64,
    /// Reserved tokens in one fixed UTC minute.
    pub tokens_per_minute: u64,
    /// Simultaneous active reservations for the route and scope.
    pub concurrent_requests: u64,
    /// Settled plus reserved tokens in the named budget period.
    pub token_budget: u64,
    /// Settled plus reserved cost, in micros, in the named budget period.
    pub cost_budget_micros: u64,
}

impl ModelAdmissionLimits {
    fn validate(self) -> Result<Self, ModelAdmissionError> {
        if self.requests_per_minute == 0
            || self.tokens_per_minute == 0
            || self.concurrent_requests == 0
            || [
                self.requests_per_minute,
                self.tokens_per_minute,
                self.concurrent_requests,
                self.token_budget,
                self.cost_budget_micros,
            ]
            .into_iter()
            .any(|value| value > MAX_SAFE_INTEGER)
        {
            return Err(ModelAdmissionError::invalid());
        }
        Ok(self)
    }

    fn stricter(self, other: Self) -> Self {
        Self {
            requests_per_minute: self.requests_per_minute.min(other.requests_per_minute),
            tokens_per_minute: self.tokens_per_minute.min(other.tokens_per_minute),
            concurrent_requests: self.concurrent_requests.min(other.concurrent_requests),
            token_budget: self.token_budget.min(other.token_budget),
            cost_budget_micros: self.cost_budget_micros.min(other.cost_budget_micros),
        }
    }
}

/// One auditable policy layer supplied by the policy authority.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelAdmissionPolicyLayer {
    authority_id: String,
    revision: u64,
    budget_period_id: String,
    route_decision: ModelRoutePolicyDecision,
    limits: ModelAdmissionLimits,
}

impl ModelAdmissionPolicyLayer {
    /// Builds one validated policy layer.
    ///
    /// # Errors
    ///
    /// Rejects malformed authority, revision, budget-period, or limit values.
    pub fn try_new(
        authority_id: String,
        revision: u64,
        budget_period_id: String,
        route_decision: ModelRoutePolicyDecision,
        limits: ModelAdmissionLimits,
    ) -> Result<Self, ModelAdmissionError> {
        validate_token(&authority_id, 200)?;
        validate_token(&budget_period_id, 200)?;
        if revision == 0 || revision > MAX_SAFE_INTEGER {
            return Err(ModelAdmissionError::invalid());
        }
        Ok(Self {
            authority_id,
            revision,
            budget_period_id,
            route_decision,
            limits: limits.validate()?,
        })
    }
}

/// Effective policy obtained by intersecting the base policy with an optional
/// enterprise ceiling. The enterprise layer can only deny a route or lower a
/// limit; it cannot be widened by Model Settings or a session preference.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenModelAdmissionPolicy {
    budget_period_id: String,
    route_allowed: bool,
    limits: ModelAdmissionLimits,
    sources: Vec<ModelPolicySource>,
    fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelPolicySource {
    /// Stable policy authority identifier.
    pub authority_id: String,
    /// Exact authority revision included in the frozen decision.
    pub revision: u64,
    /// Route decision contributed by this authority.
    pub decision: ModelRoutePolicyDecision,
}

impl FrozenModelAdmissionPolicy {
    /// Freezes the effective policy. Both layers must describe the same budget
    /// period so a smaller enterprise ceiling cannot be reset independently.
    ///
    /// # Errors
    ///
    /// Rejects an invalid layer or mismatched budget period.
    pub fn freeze(
        base: ModelAdmissionPolicyLayer,
        enterprise: Option<ModelAdmissionPolicyLayer>,
    ) -> Result<Self, ModelAdmissionError> {
        base.limits.validate()?;
        let mut route_allowed = base.route_decision == ModelRoutePolicyDecision::Allow;
        let mut limits = base.limits;
        let mut sources = vec![ModelPolicySource {
            authority_id: base.authority_id.clone(),
            revision: base.revision,
            decision: base.route_decision,
        }];
        if let Some(enterprise) = enterprise {
            enterprise.limits.validate()?;
            if enterprise.budget_period_id != base.budget_period_id {
                return Err(ModelAdmissionError::invalid());
            }
            route_allowed &= enterprise.route_decision == ModelRoutePolicyDecision::Allow;
            limits = limits.stricter(enterprise.limits);
            sources.push(ModelPolicySource {
                authority_id: enterprise.authority_id,
                revision: enterprise.revision,
                decision: enterprise.route_decision,
            });
        }
        let fingerprint_payload =
            serde_json::to_vec(&(&base.budget_period_id, route_allowed, limits, &sources))
                .map_err(|_| ModelAdmissionError::invalid())?;
        let fingerprint = format!("sha256:{:x}", Sha256::digest(fingerprint_payload));
        Ok(Self {
            budget_period_id: base.budget_period_id,
            route_allowed,
            limits,
            sources,
            fingerprint,
        })
    }

    /// Returns the effective operational limits.
    #[must_use]
    pub const fn limits(&self) -> ModelAdmissionLimits {
        self.limits
    }

    /// Returns whether every policy authority permits the route.
    #[must_use]
    pub const fn route_allowed(&self) -> bool {
        self.route_allowed
    }

    /// Returns the stable policy fingerprint retained by reservations.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Returns the budget period shared by every intersected authority.
    #[must_use]
    pub fn budget_period_id(&self) -> &str {
        &self.budget_period_id
    }

    /// Returns every auditable authority source in base-then-enterprise order.
    #[must_use]
    pub fn sources(&self) -> &[ModelPolicySource] {
        &self.sources
    }
}

/// Secret-free route authority verified from Settings, Catalog, and Credential
/// reference state before admission.
#[derive(Clone, Debug, PartialEq)]
pub struct FrozenModelRouteAuthority {
    target: ModelSettingsTarget,
    route: ModelRoute,
    route_key: ModelRequestRouteKey,
    settings_revision: u64,
    settings_concurrency_limit: u64,
    catalog_version: u64,
    provider_version: u64,
    credential_rotation_version: u64,
    fingerprint: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableFrozenModelRouteAuthority {
    target: ModelSettingsTarget,
    route: ModelRoute,
    settings_revision: u64,
    settings_concurrency_limit: u64,
    catalog_version: u64,
    provider_version: u64,
    credential_rotation_version: u64,
    fingerprint: String,
}

impl FrozenModelRouteAuthority {
    /// Joins the trusted Gateway identity with the exact Catalog and Credential
    /// resolutions that produced its Model Route.
    ///
    /// # Errors
    ///
    /// Rejects any scope, Provider, model, Credential, or version mismatch.
    pub fn from_resolved_authority(
        identity: &ProviderGatewayIdentity,
        settings: &ModelSettingsProjection,
        capability: &ResolvedModelCapability,
        credential: &CredentialReferenceResolution,
    ) -> Result<Self, ModelAdmissionError> {
        let route = settings
            .default_model_route
            .as_ref()
            .ok_or_else(ModelAdmissionError::identity_mismatch)?;
        let route_key = ModelRequestRouteKey::from_gateway(identity, route)
            .map_err(|_| ModelAdmissionError::identity_mismatch())?;
        let target_scope = target_scope(identity.target())?;
        if settings.target != *identity.target()
            || settings.worker_concurrency_limit == 0
            || settings.worker_concurrency_limit > 10_000
            || settings.revision > MAX_SAFE_INTEGER
            || credential.scope() != &capability.scope
            || !authority_scope_covers_target(&capability.scope, &target_scope)
            || capability.provider_id != route.provider_id
            || capability.model.model_id != route.model_id
            || capability.credential_reference_id != route.credential_reference_id
            || credential.credential_reference_id() != &route.credential_reference_id
            || credential.provider_id() != route.provider_id
            || capability.catalog_version == 0
            || capability.provider_version == 0
            || credential.rotation_version() == 0
            || capability.catalog_version > MAX_SAFE_INTEGER
            || capability.provider_version > MAX_SAFE_INTEGER
            || credential.rotation_version() > MAX_SAFE_INTEGER
        {
            return Err(ModelAdmissionError::identity_mismatch());
        }
        let authority = Self {
            target: identity.target().clone(),
            route: route.clone(),
            route_key,
            settings_revision: settings.revision,
            settings_concurrency_limit: settings.worker_concurrency_limit,
            catalog_version: capability.catalog_version,
            provider_version: capability.provider_version,
            credential_rotation_version: credential.rotation_version(),
            fingerprint: route_authority_fingerprint(
                identity.target(),
                route,
                settings.revision,
                settings.worker_concurrency_limit,
                capability.catalog_version,
                capability.provider_version,
                credential.rotation_version(),
            )?,
        };
        authority.validate_fingerprint()?;
        Ok(authority)
    }

    /// Returns the exact route partition also used by [`crate::ModelRequestPool`].
    #[must_use]
    pub const fn route_key(&self) -> &ModelRequestRouteKey {
        &self.route_key
    }

    /// Returns the exact durable Settings revision used for this route.
    #[must_use]
    pub const fn settings_revision(&self) -> u64 {
        self.settings_revision
    }

    /// Returns the Settings concurrency preference. Admission intersects it
    /// with the policy ceiling, so the preference can narrow but never widen.
    #[must_use]
    pub const fn settings_concurrency_limit(&self) -> u64 {
        self.settings_concurrency_limit
    }

    /// Returns the verified Catalog version.
    #[must_use]
    pub const fn catalog_version(&self) -> u64 {
        self.catalog_version
    }

    /// Returns the verified Credential rotation version.
    #[must_use]
    pub const fn credential_rotation_version(&self) -> u64 {
        self.credential_rotation_version
    }

    /// Returns the verified Provider catalog-entry version.
    #[must_use]
    pub const fn provider_version(&self) -> u64 {
        self.provider_version
    }

    /// Returns the exact secret-free Model Route.
    #[must_use]
    pub const fn route(&self) -> &ModelRoute {
        &self.route
    }

    /// Returns the exact trusted Settings target.
    #[must_use]
    pub const fn target(&self) -> &ModelSettingsTarget {
        &self.target
    }

    /// Returns the full target/route/Catalog/Credential fingerprint.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Encodes the narrow secret-free durable representation.
    ///
    /// # Errors
    ///
    /// Rejects an internally inconsistent authority or serialization failure.
    pub fn to_durable_json(&self) -> Result<Vec<u8>, ModelAdmissionError> {
        self.validate_fingerprint()?;
        serde_json::to_vec(&DurableFrozenModelRouteAuthority {
            target: self.target.clone(),
            route: self.route.clone(),
            settings_revision: self.settings_revision,
            settings_concurrency_limit: self.settings_concurrency_limit,
            catalog_version: self.catalog_version,
            provider_version: self.provider_version,
            credential_rotation_version: self.credential_rotation_version,
            fingerprint: self.fingerprint.clone(),
        })
        .map_err(|_| ModelAdmissionError::invalid())
    }

    /// Rehydrates and validates the narrow secret-free durable representation.
    ///
    /// # Errors
    ///
    /// Rejects malformed, oversized, unknown-field, or fingerprint-mismatched
    /// durable bytes.
    pub fn from_durable_json(bytes: &[u8]) -> Result<Self, ModelAdmissionError> {
        if bytes.is_empty() || bytes.len() > MAX_DURABLE_ROUTE_AUTHORITY_BYTES {
            return Err(ModelAdmissionError::invalid());
        }
        let durable: DurableFrozenModelRouteAuthority =
            serde_json::from_slice(bytes).map_err(|_| ModelAdmissionError::invalid())?;
        if serde_json::to_vec(&durable).map_err(|_| ModelAdmissionError::invalid())? != bytes {
            return Err(ModelAdmissionError::invalid());
        }
        let authority = Self {
            route_key: ModelRequestRouteKey::from_target(&durable.target, &durable.route)
                .map_err(|_| ModelAdmissionError::identity_mismatch())?,
            target: durable.target,
            route: durable.route,
            settings_revision: durable.settings_revision,
            settings_concurrency_limit: durable.settings_concurrency_limit,
            catalog_version: durable.catalog_version,
            provider_version: durable.provider_version,
            credential_rotation_version: durable.credential_rotation_version,
            fingerprint: durable.fingerprint,
        };
        authority.validate_fingerprint()?;
        Ok(authority)
    }

    /// Revalidates every persisted field and the canonical authority digest.
    ///
    /// # Errors
    ///
    /// Rejects malformed or internally inconsistent deserialized authority.
    pub fn validate_fingerprint(&self) -> Result<(), ModelAdmissionError> {
        let route_key = ModelRequestRouteKey::from_target(&self.target, &self.route)
            .map_err(|_| ModelAdmissionError::identity_mismatch())?;
        if route_key != self.route_key
            || self.settings_revision > MAX_SAFE_INTEGER
            || self.settings_concurrency_limit == 0
            || self.settings_concurrency_limit > 10_000
            || self.catalog_version == 0
            || self.catalog_version > MAX_SAFE_INTEGER
            || self.provider_version == 0
            || self.provider_version > MAX_SAFE_INTEGER
            || self.credential_rotation_version == 0
            || self.credential_rotation_version > MAX_SAFE_INTEGER
        {
            return Err(ModelAdmissionError::identity_mismatch());
        }
        let expected = route_authority_fingerprint(
            &self.target,
            &self.route,
            self.settings_revision,
            self.settings_concurrency_limit,
            self.catalog_version,
            self.provider_version,
            self.credential_rotation_version,
        )?;
        if self.fingerprint != expected {
            return Err(ModelAdmissionError::identity_mismatch());
        }
        Ok(())
    }
}

fn route_authority_fingerprint(
    target: &ModelSettingsTarget,
    route: &ModelRoute,
    settings_revision: u64,
    settings_concurrency_limit: u64,
    catalog_version: u64,
    provider_version: u64,
    credential_rotation_version: u64,
) -> Result<String, ModelAdmissionError> {
    let payload = serde_json::to_vec(&(
        target,
        route,
        settings_revision,
        settings_concurrency_limit,
        catalog_version,
        provider_version,
        credential_rotation_version,
    ))
    .map_err(|_| ModelAdmissionError::invalid())?;
    Ok(format!("sha256:{:x}", Sha256::digest(payload)))
}

/// One pre-Provider reservation request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelReservationRequest {
    admission: ModelRequestAdmission,
    estimated_tokens: u64,
    estimated_cost_micros: u64,
}

impl ModelReservationRequest {
    /// Builds one bounded reservation from the same admission consumed by the
    /// in-memory request pool.
    ///
    /// # Errors
    ///
    /// Rejects a zero or unsafe token/cost estimate.
    pub fn try_new(
        admission: ModelRequestAdmission,
        estimated_tokens: u64,
        estimated_cost_micros: u64,
    ) -> Result<Self, ModelAdmissionError> {
        if estimated_tokens == 0
            || estimated_tokens > MAX_SAFE_INTEGER
            || estimated_cost_micros > MAX_SAFE_INTEGER
        {
            return Err(ModelAdmissionError::invalid());
        }
        Ok(Self {
            admission,
            estimated_tokens,
            estimated_cost_micros,
        })
    }

    /// Returns the exchange identity used as the reservation key.
    #[must_use]
    pub const fn model_exchange_id(&self) -> &ModelExchangeId {
        &self.admission.model_exchange_id
    }
}

/// One cancellation or Provider-failure release category.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReservationReleaseReason {
    /// The caller cancelled before successful completion.
    Cancelled,
    /// Provider open or streaming failed.
    ProviderFailed,
}

/// Command which releases one active reservation exactly once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelReservationRelease {
    /// Idempotency identity for this terminal operation.
    pub request_id: RequestId,
    /// Active exchange to release.
    pub model_exchange_id: ModelExchangeId,
    /// Terminal release category.
    pub reason: ModelReservationReleaseReason,
}

/// Successful usage and cost settlement for one active reservation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelReservationCompletion {
    /// Idempotency identity for this terminal operation.
    pub request_id: RequestId,
    /// Active exchange to settle.
    pub model_exchange_id: ModelExchangeId,
    /// Provider-normalized token usage.
    pub usage: ProviderTokenUsage,
    /// Provider-normalized cost in micros; organization billing consumes this fact later.
    pub actual_cost_micros: u64,
}

/// Stable reason for a durable admission denial.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAdmissionDenialReason {
    /// A base or enterprise policy denied the route.
    PolicyDenied,
    /// The fixed-minute request limit was exhausted.
    RequestsPerMinute,
    /// The fixed-minute token limit was exhausted.
    TokensPerMinute,
    /// The route/scope concurrency ceiling was exhausted.
    Concurrency,
    /// The operational token budget was exhausted.
    TokenBudget,
    /// The operational cost budget was exhausted.
    CostBudget,
}

/// Durable reservation result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelReservationReceipt {
    /// Original reservation request identity.
    pub request_id: RequestId,
    /// Exact exchange reservation identity.
    pub model_exchange_id: ModelExchangeId,
    /// Fingerprint of the exact frozen route authority admitted for this exchange.
    pub route_authority_fingerprint: String,
    /// Denial reason, or `None` when admitted.
    pub denial: Option<ModelAdmissionDenialReason>,
    /// Fixed UTC minute observed by the first durable execution.
    pub unix_minute: u64,
    /// Durable ledger revision.
    pub revision: u64,
    /// Whether this value came from the original durable receipt.
    pub idempotent_replay: bool,
}

impl ModelReservationReceipt {
    /// Returns whether Provider invocation may start.
    #[must_use]
    pub const fn admitted(&self) -> bool {
        self.denial.is_none()
    }
}

/// Terminal reservation state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReservationTerminalOutcome {
    /// Reservation released because the caller cancelled.
    Cancelled,
    /// Reservation released because Provider processing failed.
    ProviderFailed,
    /// Reservation settled with actual usage.
    Completed,
}

/// Durable release or settlement result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelReservationTerminalReceipt {
    /// Terminal operation idempotency identity.
    pub request_id: RequestId,
    /// Exact exchange reservation identity.
    pub model_exchange_id: ModelExchangeId,
    /// Fingerprint of the exact frozen route authority settled for this exchange.
    pub route_authority_fingerprint: String,
    /// Applied terminal outcome.
    pub outcome: ModelReservationTerminalOutcome,
    /// Actual settled token count; zero for a release.
    pub actual_tokens: u64,
    /// Actual settled cost in micros; zero for a release.
    pub actual_cost_micros: u64,
    /// Durable ledger revision.
    pub revision: u64,
    /// Whether this value came from the original durable receipt.
    pub idempotent_replay: bool,
}

/// Read-only operational ledger projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAdmissionSnapshot {
    /// Active reservations across policy revisions and budget periods.
    pub active_reservations: u64,
    /// Requests accepted in the current retained fixed-minute window.
    pub minute_requests: u64,
    /// Tokens currently counted in that fixed-minute window.
    pub minute_tokens: u64,
    /// Tokens reserved but not settled in the requested budget period.
    pub budget_reserved_tokens: u64,
    /// Cost reserved but not settled in the requested budget period.
    pub budget_reserved_cost_micros: u64,
    /// Actual tokens settled in the requested budget period.
    pub budget_settled_tokens: u64,
    /// Actual cost settled in the requested budget period.
    pub budget_settled_cost_micros: u64,
    /// Current durable ledger revision.
    pub revision: u64,
}

/// Stable model-admission failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelAdmissionErrorKind {
    /// Caller supplied malformed input.
    InvalidRequest,
    /// Trusted route, Catalog, Credential, or scope facts did not match.
    IdentityMismatch,
    /// Exchange identity was reused for another reservation.
    ReservationConflict,
    /// Terminal command named no active or retained reservation.
    ReservationNotFound,
    /// Reservation already reached another terminal outcome.
    TerminalConflict,
    /// Scoped request identity was reused with changed input.
    RequestConflict,
    /// Persisted admission state or receipt was invalid.
    CorruptState,
    /// Authoritative clock was unavailable.
    ClockUnavailable,
    /// Durable storage failed.
    Storage,
}

/// Bounded model-admission error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAdmissionError {
    kind: ModelAdmissionErrorKind,
    message: &'static str,
}

impl ModelAdmissionError {
    const fn new(kind: ModelAdmissionErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            ModelAdmissionErrorKind::InvalidRequest,
            "model admission request is invalid",
        )
    }

    const fn identity_mismatch() -> Self {
        Self::new(
            ModelAdmissionErrorKind::IdentityMismatch,
            "model admission authority does not match the route and scope",
        )
    }

    const fn corrupt() -> Self {
        Self::new(
            ModelAdmissionErrorKind::CorruptState,
            "model admission durable state is invalid",
        )
    }

    /// Returns the stable machine-readable category.
    #[must_use]
    pub const fn kind(&self) -> ModelAdmissionErrorKind {
        self.kind
    }
}

impl fmt::Display for ModelAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ModelAdmissionError {}

impl From<ModelAdmissionClockError> for ModelAdmissionError {
    fn from(_error: ModelAdmissionClockError) -> Self {
        Self::new(
            ModelAdmissionErrorKind::ClockUnavailable,
            "model admission clock is unavailable",
        )
    }
}

impl From<StorageError> for ModelAdmissionError {
    fn from(error: StorageError) -> Self {
        match error.kind() {
            StorageErrorKind::RequestConflict => Self::new(
                ModelAdmissionErrorKind::RequestConflict,
                "model admission request identity was reused with changed input",
            ),
            StorageErrorKind::InvalidInput => Self::invalid(),
            StorageErrorKind::RevisionConflict
            | StorageErrorKind::RequestReplayMissing
            | StorageErrorKind::JournalAlreadyExists
            | StorageErrorKind::JournalNotFound
            | StorageErrorKind::JournalConflict
            | StorageErrorKind::EventCursorExpired
            | StorageErrorKind::Adapter
            | StorageErrorKind::Closed => Self::new(
                ModelAdmissionErrorKind::Storage,
                "model admission storage operation failed",
            ),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RouteBinding {
    target: ModelSettingsTarget,
    route: ModelRoute,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MinuteLedger {
    unix_minute: u64,
    requests: u64,
    tokens: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BudgetLedger {
    reserved_tokens: u64,
    reserved_cost_micros: u64,
    settled_tokens: u64,
    settled_cost_micros: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActiveReservation {
    request_id: RequestId,
    authority_fingerprint: String,
    policy_fingerprint: String,
    budget_period_id: String,
    admitted_minute: u64,
    estimated_tokens: u64,
    estimated_cost_micros: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TerminalReservation {
    outcome: ModelReservationTerminalOutcome,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelAdmissionState {
    schema: String,
    binding: RouteBinding,
    revision: u64,
    minute: Option<MinuteLedger>,
    budgets: BTreeMap<String, BudgetLedger>,
    active: BTreeMap<String, ActiveReservation>,
    terminal: BTreeMap<String, TerminalReservation>,
}

impl ModelAdmissionState {
    fn empty(authority: &FrozenModelRouteAuthority) -> Self {
        Self {
            schema: STATE_SCHEMA.to_owned(),
            binding: RouteBinding {
                target: authority.target.clone(),
                route: authority.route.clone(),
            },
            revision: 0,
            minute: None,
            budgets: BTreeMap::new(),
            active: BTreeMap::new(),
            terminal: BTreeMap::new(),
        }
    }

    fn roll_minute(&mut self, unix_minute: u64) -> Result<(), ModelAdmissionError> {
        if self
            .minute
            .as_ref()
            .is_some_and(|minute| unix_minute < minute.unix_minute)
        {
            return Err(ModelAdmissionError::new(
                ModelAdmissionErrorKind::ClockUnavailable,
                "model admission clock moved backward",
            ));
        }
        if self.minute.as_ref().map(|minute| minute.unix_minute) != Some(unix_minute) {
            self.minute = Some(MinuteLedger {
                unix_minute,
                requests: 0,
                tokens: 0,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", content = "receipt", rename_all = "snake_case")]
enum ModelAdmissionEvent {
    Reserved(ModelReservationReceipt),
    Terminal(ModelReservationTerminalReceipt),
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum CommandDigest<'a> {
    Reserve {
        authority: AuthorityDigest<'a>,
        policy: &'a FrozenModelAdmissionPolicy,
        request: ReservationDigest<'a>,
    },
    Release {
        authority: AuthorityDigest<'a>,
        request: ReleaseDigest<'a>,
    },
    Complete {
        authority: AuthorityDigest<'a>,
        request: CompletionDigest<'a>,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorityDigest<'a> {
    target: &'a ModelSettingsTarget,
    route: &'a ModelRoute,
    settings_revision: u64,
    settings_concurrency_limit: u64,
    catalog_version: u64,
    provider_version: u64,
    credential_rotation_version: u64,
    fingerprint: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReservationDigest<'a> {
    request_id: &'a RequestId,
    model_exchange_id: &'a ModelExchangeId,
    estimated_tokens: u64,
    estimated_cost_micros: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseDigest<'a> {
    request_id: &'a RequestId,
    model_exchange_id: &'a ModelExchangeId,
    reason: ModelReservationReleaseReason,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompletionDigest<'a> {
    request_id: &'a RequestId,
    model_exchange_id: &'a ModelExchangeId,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    actual_cost_micros: u64,
}

struct CommandReceipt {
    identity: ReceiptIdentity,
    digest: Sha256Digest,
}

/// Durable model admission service. It must return an admitted receipt before
/// the caller enters the Provider Gateway.
pub struct ModelAdmissionService<'a> {
    storage: &'a mut dyn ProductStateStorage,
    clock: &'a dyn ModelAdmissionClock,
}

impl<'a> ModelAdmissionService<'a> {
    /// Builds one service over the canonical state storage and authoritative clock.
    #[must_use]
    pub fn new(
        storage: &'a mut dyn ProductStateStorage,
        clock: &'a dyn ModelAdmissionClock,
    ) -> Self {
        Self { storage, clock }
    }

    /// Atomically applies route policy, RPM, TPM, concurrency, and operational
    /// token/cost budgets before Provider invocation.
    ///
    /// # Errors
    ///
    /// Rejects invalid authority/input, changed-body replay, corrupt state, or
    /// unavailable clock/storage. Limit exhaustion is a durable denial receipt.
    pub fn reserve(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        policy: &FrozenModelAdmissionPolicy,
        request: &ModelReservationRequest,
    ) -> Result<ModelReservationReceipt, ModelAdmissionError> {
        validate_reservation(authority, policy, request)?;
        let command = reserve_command(authority, policy, request)?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&command.identity, &command.digest)?
        {
            return reservation_from_receipt(&receipt, authority, true);
        }
        let unix_minute = self.clock.unix_minute()?;
        if unix_minute > MAX_SAFE_INTEGER {
            return Err(ModelAdmissionError::invalid());
        }
        for _attempt in 0..MAX_COMMIT_RETRIES {
            if let Some(receipt) = self
                .storage
                .load_receipt(&command.identity, &command.digest)?
            {
                return reservation_from_receipt(&receipt, authority, true);
            }
            let (mut state, expected_revision) = load_state(self.storage, authority)?;
            if let Some(existing) = state.active.get(&request.admission.model_exchange_id.0) {
                if reservation_matches(existing, authority, policy, request) {
                    let receipt = self
                        .storage
                        .load_receipt(&command.identity, &command.digest)?
                        .ok_or_else(ModelAdmissionError::corrupt)?;
                    return reservation_from_receipt(&receipt, authority, true);
                }
                return Err(ModelAdmissionError::new(
                    ModelAdmissionErrorKind::ReservationConflict,
                    "model exchange identity was reused for another reservation",
                ));
            }
            state.roll_minute(unix_minute)?;
            let denial = admission_denial(&state, authority, policy, request)?;
            if denial.is_none() {
                apply_reservation(&mut state, authority, policy, request, unix_minute)?;
            }
            let revision = next_revision(expected_revision)?;
            state.revision = revision;
            let result = ModelReservationReceipt {
                request_id: request.admission.request_id.clone(),
                model_exchange_id: request.admission.model_exchange_id.clone(),
                route_authority_fingerprint: authority.fingerprint().to_owned(),
                denial,
                unix_minute,
                revision,
                idempotent_replay: false,
            };
            match commit_event(
                self.storage,
                authority,
                &command,
                expected_revision,
                &state,
                &ModelAdmissionEvent::Reserved(result.clone()),
            ) {
                Ok(receipt) => {
                    return reservation_from_receipt(
                        &receipt,
                        authority,
                        receipt.idempotent_replay,
                    );
                }
                Err(error) => {
                    if error.kind() != StorageErrorKind::RevisionConflict {
                        return Err(error.into());
                    }
                }
            }
        }
        Err(ModelAdmissionError::new(
            ModelAdmissionErrorKind::Storage,
            "model admission concurrency retry limit was exhausted",
        ))
    }

    /// Releases a cancelled or failed reservation once and restores its active,
    /// token, and cost capacity. The accepted RPM count remains consumed.
    ///
    /// # Errors
    ///
    /// Rejects invalid input, an unknown/conflicting terminal, corrupt state,
    /// changed-body replay, or storage failure.
    pub fn release(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        request: &ModelReservationRelease,
    ) -> Result<ModelReservationTerminalReceipt, ModelAdmissionError> {
        validate_request_id(&request.request_id)?;
        validate_model_exchange_id(&request.model_exchange_id)?;
        let command = release_command(authority, request)?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&command.identity, &command.digest)?
        {
            return terminal_from_receipt(&receipt, authority, true);
        }
        self.apply_terminal(
            authority,
            &command,
            &request.model_exchange_id,
            |state, active| {
                let outcome = match request.reason {
                    ModelReservationReleaseReason::Cancelled => {
                        ModelReservationTerminalOutcome::Cancelled
                    }
                    ModelReservationReleaseReason::ProviderFailed => {
                        ModelReservationTerminalOutcome::ProviderFailed
                    }
                };
                release_capacity(state, active)?;
                Ok((outcome, 0, 0))
            },
        )
    }

    /// Releases an exact reservation when present; absence is a safe no-op for
    /// recovery before the reserve side effect began.
    ///
    /// # Errors
    ///
    /// Rejects foreign authority, changed terminal replay, corrupt state, or
    /// storage failure. An existing exact terminal receipt is replayed.
    pub fn release_if_reserved(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        request: &ModelReservationRelease,
    ) -> Result<Option<ModelReservationTerminalReceipt>, ModelAdmissionError> {
        validate_request_id(&request.request_id)?;
        validate_model_exchange_id(&request.model_exchange_id)?;
        let command = release_command(authority, request)?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&command.identity, &command.digest)?
        {
            return terminal_from_receipt(&receipt, authority, true).map(Some);
        }
        let (state, _revision) = load_state(self.storage, authority)?;
        if let Some(active) = state.active.get(&request.model_exchange_id.0) {
            if active.authority_fingerprint != authority.fingerprint {
                return Err(ModelAdmissionError::identity_mismatch());
            }
            return self.release(authority, request).map(Some);
        }
        if state.terminal.contains_key(&request.model_exchange_id.0) {
            return Err(ModelAdmissionError::new(
                ModelAdmissionErrorKind::TerminalConflict,
                "model reservation already reached another terminal outcome",
            ));
        }
        Ok(None)
    }

    /// Settles actual Provider usage once, releases concurrency, and replaces
    /// the estimate with actual token/cost facts. The returned usage is an input
    /// to the separate organization billing authority.
    ///
    /// # Errors
    ///
    /// Rejects invalid usage, an unknown/conflicting terminal, corrupt state,
    /// changed-body replay, or storage failure.
    pub fn complete(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        request: &ModelReservationCompletion,
    ) -> Result<ModelReservationTerminalReceipt, ModelAdmissionError> {
        validate_request_id(&request.request_id)?;
        validate_model_exchange_id(&request.model_exchange_id)?;
        let actual_tokens = total_tokens(request.usage)?;
        if request.actual_cost_micros > MAX_SAFE_INTEGER {
            return Err(ModelAdmissionError::invalid());
        }
        let command = completion_command(authority, request)?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&command.identity, &command.digest)?
        {
            return terminal_from_receipt(&receipt, authority, true);
        }
        self.apply_terminal(
            authority,
            &command,
            &request.model_exchange_id,
            |state, active| {
                settle_capacity(state, active, actual_tokens, request.actual_cost_micros)?;
                Ok((
                    ModelReservationTerminalOutcome::Completed,
                    actual_tokens,
                    request.actual_cost_micros,
                ))
            },
        )
    }

    /// Reads the route/scope ledger for one budget period without mutation.
    ///
    /// # Errors
    ///
    /// Returns invalid/corrupt state or storage failures.
    pub fn snapshot(
        &self,
        authority: &FrozenModelRouteAuthority,
        budget_period_id: &str,
    ) -> Result<ModelAdmissionSnapshot, ModelAdmissionError> {
        validate_token(budget_period_id, 200)?;
        let (state, _revision) = load_state(self.storage, authority)?;
        let minute = state.minute.unwrap_or(MinuteLedger {
            unix_minute: 0,
            requests: 0,
            tokens: 0,
        });
        let budget = state
            .budgets
            .get(budget_period_id)
            .cloned()
            .unwrap_or_default();
        Ok(ModelAdmissionSnapshot {
            active_reservations: u64::try_from(state.active.len())
                .map_err(|_| ModelAdmissionError::corrupt())?,
            minute_requests: minute.requests,
            minute_tokens: minute.tokens,
            budget_reserved_tokens: budget.reserved_tokens,
            budget_reserved_cost_micros: budget.reserved_cost_micros,
            budget_settled_tokens: budget.settled_tokens,
            budget_settled_cost_micros: budget.settled_cost_micros,
            revision: state.revision,
        })
    }

    fn apply_terminal<F>(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        command: &CommandReceipt,
        model_exchange_id: &ModelExchangeId,
        apply: F,
    ) -> Result<ModelReservationTerminalReceipt, ModelAdmissionError>
    where
        F: Fn(
            &mut ModelAdmissionState,
            &ActiveReservation,
        ) -> Result<(ModelReservationTerminalOutcome, u64, u64), ModelAdmissionError>,
    {
        for _attempt in 0..MAX_COMMIT_RETRIES {
            if let Some(receipt) = self
                .storage
                .load_receipt(&command.identity, &command.digest)?
            {
                return terminal_from_receipt(&receipt, authority, true);
            }
            let (mut state, expected_revision) = load_state(self.storage, authority)?;
            if state.terminal.contains_key(&model_exchange_id.0) {
                if let Some(receipt) = self
                    .storage
                    .load_receipt(&command.identity, &command.digest)?
                {
                    return terminal_from_receipt(&receipt, authority, true);
                }
                return Err(ModelAdmissionError::new(
                    ModelAdmissionErrorKind::TerminalConflict,
                    "model reservation already reached a terminal outcome",
                ));
            }
            let active = state
                .active
                .get(&model_exchange_id.0)
                .ok_or_else(|| {
                    ModelAdmissionError::new(
                        ModelAdmissionErrorKind::ReservationNotFound,
                        "model reservation is not active",
                    )
                })?
                .clone();
            if active.authority_fingerprint != authority.fingerprint {
                return Err(ModelAdmissionError::identity_mismatch());
            }
            state.active.remove(&model_exchange_id.0);
            let (outcome, actual_tokens, actual_cost_micros) = apply(&mut state, &active)?;
            state
                .terminal
                .insert(model_exchange_id.0.clone(), TerminalReservation { outcome });
            let revision = next_revision(expected_revision)?;
            state.revision = revision;
            let result = ModelReservationTerminalReceipt {
                request_id: command.identity.request_id().clone(),
                model_exchange_id: model_exchange_id.clone(),
                route_authority_fingerprint: authority.fingerprint().to_owned(),
                outcome,
                actual_tokens,
                actual_cost_micros,
                revision,
                idempotent_replay: false,
            };
            match commit_event(
                self.storage,
                authority,
                command,
                expected_revision,
                &state,
                &ModelAdmissionEvent::Terminal(result.clone()),
            ) {
                Ok(receipt) => {
                    return terminal_from_receipt(&receipt, authority, receipt.idempotent_replay);
                }
                Err(error) => {
                    if error.kind() != StorageErrorKind::RevisionConflict {
                        return Err(error.into());
                    }
                }
            }
        }
        Err(ModelAdmissionError::new(
            ModelAdmissionErrorKind::Storage,
            "model admission concurrency retry limit was exhausted",
        ))
    }
}

fn validate_reservation(
    authority: &FrozenModelRouteAuthority,
    policy: &FrozenModelAdmissionPolicy,
    request: &ModelReservationRequest,
) -> Result<(), ModelAdmissionError> {
    validate_request_id(&request.admission.request_id)?;
    validate_model_exchange_id(&request.admission.model_exchange_id)?;
    if request.admission.route != authority.route_key
        || policy.budget_period_id.is_empty()
        || policy.fingerprint.is_empty()
        || request.estimated_tokens == 0
        || request.estimated_tokens > MAX_SAFE_INTEGER
        || request.estimated_cost_micros > MAX_SAFE_INTEGER
    {
        return Err(ModelAdmissionError::identity_mismatch());
    }
    policy.limits.validate()?;
    Ok(())
}

fn admission_denial(
    state: &ModelAdmissionState,
    authority: &FrozenModelRouteAuthority,
    policy: &FrozenModelAdmissionPolicy,
    request: &ModelReservationRequest,
) -> Result<Option<ModelAdmissionDenialReason>, ModelAdmissionError> {
    if state
        .terminal
        .contains_key(&request.admission.model_exchange_id.0)
    {
        return Err(ModelAdmissionError::new(
            ModelAdmissionErrorKind::ReservationConflict,
            "terminal model exchange identity cannot be reserved again",
        ));
    }
    if !policy.route_allowed {
        return Ok(Some(ModelAdmissionDenialReason::PolicyDenied));
    }
    let minute = state
        .minute
        .as_ref()
        .ok_or_else(ModelAdmissionError::corrupt)?;
    if minute.requests >= policy.limits.requests_per_minute {
        return Ok(Some(ModelAdmissionDenialReason::RequestsPerMinute));
    }
    if checked_add(minute.tokens, request.estimated_tokens)? > policy.limits.tokens_per_minute {
        return Ok(Some(ModelAdmissionDenialReason::TokensPerMinute));
    }
    if u64::try_from(state.active.len()).map_err(|_| ModelAdmissionError::corrupt())?
        >= policy
            .limits
            .concurrent_requests
            .min(authority.settings_concurrency_limit)
    {
        return Ok(Some(ModelAdmissionDenialReason::Concurrency));
    }
    let budget = state
        .budgets
        .get(&policy.budget_period_id)
        .cloned()
        .unwrap_or_default();
    let total_tokens = checked_add(
        checked_add(budget.reserved_tokens, budget.settled_tokens)?,
        request.estimated_tokens,
    )?;
    if total_tokens > policy.limits.token_budget {
        return Ok(Some(ModelAdmissionDenialReason::TokenBudget));
    }
    let total_cost = checked_add(
        checked_add(budget.reserved_cost_micros, budget.settled_cost_micros)?,
        request.estimated_cost_micros,
    )?;
    if total_cost > policy.limits.cost_budget_micros {
        return Ok(Some(ModelAdmissionDenialReason::CostBudget));
    }
    Ok(None)
}

fn reservation_matches(
    existing: &ActiveReservation,
    authority: &FrozenModelRouteAuthority,
    policy: &FrozenModelAdmissionPolicy,
    request: &ModelReservationRequest,
) -> bool {
    existing.request_id == request.admission.request_id
        && existing.authority_fingerprint == authority.fingerprint
        && existing.policy_fingerprint == policy.fingerprint
        && existing.budget_period_id == policy.budget_period_id
        && existing.estimated_tokens == request.estimated_tokens
        && existing.estimated_cost_micros == request.estimated_cost_micros
}

fn apply_reservation(
    state: &mut ModelAdmissionState,
    authority: &FrozenModelRouteAuthority,
    policy: &FrozenModelAdmissionPolicy,
    request: &ModelReservationRequest,
    unix_minute: u64,
) -> Result<(), ModelAdmissionError> {
    let minute = state
        .minute
        .as_mut()
        .ok_or_else(ModelAdmissionError::corrupt)?;
    minute.requests = checked_add(minute.requests, 1)?;
    minute.tokens = checked_add(minute.tokens, request.estimated_tokens)?;
    let budget = state
        .budgets
        .entry(policy.budget_period_id.clone())
        .or_default();
    budget.reserved_tokens = checked_add(budget.reserved_tokens, request.estimated_tokens)?;
    budget.reserved_cost_micros =
        checked_add(budget.reserved_cost_micros, request.estimated_cost_micros)?;
    state.active.insert(
        request.admission.model_exchange_id.0.clone(),
        ActiveReservation {
            request_id: request.admission.request_id.clone(),
            authority_fingerprint: authority.fingerprint.clone(),
            policy_fingerprint: policy.fingerprint.clone(),
            budget_period_id: policy.budget_period_id.clone(),
            admitted_minute: unix_minute,
            estimated_tokens: request.estimated_tokens,
            estimated_cost_micros: request.estimated_cost_micros,
        },
    );
    Ok(())
}

fn release_capacity(
    state: &mut ModelAdmissionState,
    active: &ActiveReservation,
) -> Result<(), ModelAdmissionError> {
    if state
        .minute
        .as_ref()
        .is_some_and(|minute| minute.unix_minute == active.admitted_minute)
    {
        let minute = state
            .minute
            .as_mut()
            .ok_or_else(ModelAdmissionError::corrupt)?;
        minute.tokens = checked_sub(minute.tokens, active.estimated_tokens)?;
    }
    let budget = state
        .budgets
        .get_mut(&active.budget_period_id)
        .ok_or_else(ModelAdmissionError::corrupt)?;
    budget.reserved_tokens = checked_sub(budget.reserved_tokens, active.estimated_tokens)?;
    budget.reserved_cost_micros =
        checked_sub(budget.reserved_cost_micros, active.estimated_cost_micros)?;
    Ok(())
}

fn settle_capacity(
    state: &mut ModelAdmissionState,
    active: &ActiveReservation,
    actual_tokens: u64,
    actual_cost_micros: u64,
) -> Result<(), ModelAdmissionError> {
    if state
        .minute
        .as_ref()
        .is_some_and(|minute| minute.unix_minute == active.admitted_minute)
    {
        let minute = state
            .minute
            .as_mut()
            .ok_or_else(ModelAdmissionError::corrupt)?;
        minute.tokens = checked_sub(minute.tokens, active.estimated_tokens)?;
        minute.tokens = checked_add(minute.tokens, actual_tokens)?;
    }
    let budget = state
        .budgets
        .get_mut(&active.budget_period_id)
        .ok_or_else(ModelAdmissionError::corrupt)?;
    budget.reserved_tokens = checked_sub(budget.reserved_tokens, active.estimated_tokens)?;
    budget.reserved_cost_micros =
        checked_sub(budget.reserved_cost_micros, active.estimated_cost_micros)?;
    budget.settled_tokens = checked_add(budget.settled_tokens, actual_tokens)?;
    budget.settled_cost_micros = checked_add(budget.settled_cost_micros, actual_cost_micros)?;
    Ok(())
}

fn total_tokens(usage: ProviderTokenUsage) -> Result<u64, ModelAdmissionError> {
    if usage.cached_input_tokens > usage.input_tokens
        || usage.reasoning_output_tokens > usage.output_tokens
        || usage.cache_write_input_tokens > usage.input_tokens
    {
        return Err(ModelAdmissionError::invalid());
    }
    let total = checked_add(usage.input_tokens, usage.output_tokens)?;
    if total > MAX_SAFE_INTEGER {
        return Err(ModelAdmissionError::invalid());
    }
    Ok(total)
}

fn load_state(
    storage: &dyn ProductStateStorage,
    authority: &FrozenModelRouteAuthority,
) -> Result<(ModelAdmissionState, u64), ModelAdmissionError> {
    let stream_id = stream_id(authority)?;
    let Some(stored) = storage.load_state(&stream_id)? else {
        return Ok((ModelAdmissionState::empty(authority), 0));
    };
    decode_state(&stored, authority)
}

fn decode_state(
    stored: &StoredState,
    authority: &FrozenModelRouteAuthority,
) -> Result<(ModelAdmissionState, u64), ModelAdmissionError> {
    let expected_stream_id = stream_id(authority)?;
    let state: ModelAdmissionState =
        serde_json::from_slice(&stored.payload).map_err(|_| ModelAdmissionError::corrupt())?;
    let canonical = serde_json::to_vec(&state).map_err(|_| ModelAdmissionError::corrupt())?;
    if canonical != stored.payload
        || stored.stream_id != expected_stream_id
        || state.schema != STATE_SCHEMA
        || state.revision != stored.revision
        || state.binding.target != authority.target
        || state.binding.route != authority.route
        || state.revision == 0
        || state.revision > MAX_SAFE_INTEGER
    {
        return Err(ModelAdmissionError::corrupt());
    }
    validate_state_ledgers(&state)?;
    Ok((state, stored.revision))
}

fn validate_state_ledgers(state: &ModelAdmissionState) -> Result<(), ModelAdmissionError> {
    if state.active.values().any(|reservation| {
        reservation.request_id.0.is_empty()
            || reservation.authority_fingerprint.is_empty()
            || reservation.policy_fingerprint.is_empty()
            || reservation.budget_period_id.is_empty()
            || reservation.estimated_tokens == 0
            || reservation.estimated_tokens > MAX_SAFE_INTEGER
            || reservation.estimated_cost_micros > MAX_SAFE_INTEGER
            || !state.budgets.contains_key(&reservation.budget_period_id)
    }) {
        return Err(ModelAdmissionError::corrupt());
    }
    if state
        .active
        .keys()
        .any(|exchange| state.terminal.contains_key(exchange))
    {
        return Err(ModelAdmissionError::corrupt());
    }
    let reserved_by_period = state.active.values().try_fold(
        BTreeMap::<&str, (u64, u64)>::new(),
        |mut totals, reservation| {
            let entry = totals
                .entry(&reservation.budget_period_id)
                .or_insert((0, 0));
            entry.0 = checked_add(entry.0, reservation.estimated_tokens)?;
            entry.1 = checked_add(entry.1, reservation.estimated_cost_micros)?;
            Ok::<_, ModelAdmissionError>(totals)
        },
    )?;
    for (period, ledger) in &state.budgets {
        let expected = reserved_by_period
            .get(period.as_str())
            .copied()
            .unwrap_or((0, 0));
        if (ledger.reserved_tokens, ledger.reserved_cost_micros) != expected
            || [
                ledger.reserved_tokens,
                ledger.reserved_cost_micros,
                ledger.settled_tokens,
                ledger.settled_cost_micros,
            ]
            .into_iter()
            .any(|value| value > MAX_SAFE_INTEGER)
        {
            return Err(ModelAdmissionError::corrupt());
        }
    }
    Ok(())
}

fn commit_event(
    storage: &mut dyn ProductStateStorage,
    authority: &FrozenModelRouteAuthority,
    command: &CommandReceipt,
    expected_revision: u64,
    state: &ModelAdmissionState,
    event: &ModelAdmissionEvent,
) -> Result<CommitReceipt, StorageError> {
    let state_payload = serde_json::to_vec(state)
        .map_err(|_| StorageError::invalid_input("model admission state serialization failed"))?;
    let event_payload = serde_json::to_vec(&(EVENT_SCHEMA, &event))
        .map_err(|_| StorageError::invalid_input("model admission event serialization failed"))?;
    let event_id = format!(
        "model-admission:{:x}",
        Sha256::digest(
            [
                command.identity.request_id().0.as_bytes(),
                command.digest.0.as_bytes(),
                event_payload.as_slice(),
            ]
            .concat()
        )
    );
    storage.commit(&StateCommit::new(
        command.identity.clone(),
        command.digest.clone(),
        stream_id(authority).map_err(|_| {
            StorageError::invalid_input("model admission stream identity is invalid")
        })?,
        expected_revision,
        state_payload,
        vec![NewOutboxEvent::internal(
            event_id,
            EVENT_TOPIC,
            event_payload,
        )],
    ))
}

fn reservation_from_receipt(
    receipt: &CommitReceipt,
    authority: &FrozenModelRouteAuthority,
    replay: bool,
) -> Result<ModelReservationReceipt, ModelAdmissionError> {
    match event_from_receipt(receipt)? {
        ModelAdmissionEvent::Reserved(mut result) => {
            if result.route_authority_fingerprint != authority.fingerprint() {
                return Err(ModelAdmissionError::corrupt());
            }
            result.idempotent_replay = replay;
            Ok(result)
        }
        ModelAdmissionEvent::Terminal(_) => Err(ModelAdmissionError::corrupt()),
    }
}

fn terminal_from_receipt(
    receipt: &CommitReceipt,
    authority: &FrozenModelRouteAuthority,
    replay: bool,
) -> Result<ModelReservationTerminalReceipt, ModelAdmissionError> {
    match event_from_receipt(receipt)? {
        ModelAdmissionEvent::Terminal(mut result) => {
            if result.route_authority_fingerprint != authority.fingerprint() {
                return Err(ModelAdmissionError::corrupt());
            }
            result.idempotent_replay = replay;
            Ok(result)
        }
        ModelAdmissionEvent::Reserved(_) => Err(ModelAdmissionError::corrupt()),
    }
}

fn event_from_receipt(receipt: &CommitReceipt) -> Result<ModelAdmissionEvent, ModelAdmissionError> {
    let [event] = receipt.events.as_slice() else {
        return Err(ModelAdmissionError::corrupt());
    };
    if event.topic != EVENT_TOPIC {
        return Err(ModelAdmissionError::corrupt());
    }
    let (schema, decoded): (String, ModelAdmissionEvent) =
        serde_json::from_slice(&event.payload).map_err(|_| ModelAdmissionError::corrupt())?;
    if schema != EVENT_SCHEMA
        || serde_json::to_vec(&(schema, &decoded)).map_err(|_| ModelAdmissionError::corrupt())?
            != event.payload
    {
        return Err(ModelAdmissionError::corrupt());
    }
    Ok(decoded)
}

fn reserve_command(
    authority: &FrozenModelRouteAuthority,
    policy: &FrozenModelAdmissionPolicy,
    request: &ModelReservationRequest,
) -> Result<CommandReceipt, ModelAdmissionError> {
    command_receipt(
        authority,
        &request.admission.request_id,
        &CommandDigest::Reserve {
            authority: authority_digest(authority),
            policy,
            request: ReservationDigest {
                request_id: &request.admission.request_id,
                model_exchange_id: &request.admission.model_exchange_id,
                estimated_tokens: request.estimated_tokens,
                estimated_cost_micros: request.estimated_cost_micros,
            },
        },
    )
}

fn release_command(
    authority: &FrozenModelRouteAuthority,
    request: &ModelReservationRelease,
) -> Result<CommandReceipt, ModelAdmissionError> {
    command_receipt(
        authority,
        &request.request_id,
        &CommandDigest::Release {
            authority: authority_digest(authority),
            request: ReleaseDigest {
                request_id: &request.request_id,
                model_exchange_id: &request.model_exchange_id,
                reason: request.reason,
            },
        },
    )
}

fn completion_command(
    authority: &FrozenModelRouteAuthority,
    request: &ModelReservationCompletion,
) -> Result<CommandReceipt, ModelAdmissionError> {
    command_receipt(
        authority,
        &request.request_id,
        &CommandDigest::Complete {
            authority: authority_digest(authority),
            request: CompletionDigest {
                request_id: &request.request_id,
                model_exchange_id: &request.model_exchange_id,
                input_tokens: request.usage.input_tokens,
                cached_input_tokens: request.usage.cached_input_tokens,
                cache_write_input_tokens: request.usage.cache_write_input_tokens,
                output_tokens: request.usage.output_tokens,
                reasoning_output_tokens: request.usage.reasoning_output_tokens,
                actual_cost_micros: request.actual_cost_micros,
            },
        },
    )
}

fn authority_digest(authority: &FrozenModelRouteAuthority) -> AuthorityDigest<'_> {
    AuthorityDigest {
        target: &authority.target,
        route: &authority.route,
        settings_revision: authority.settings_revision,
        settings_concurrency_limit: authority.settings_concurrency_limit,
        catalog_version: authority.catalog_version,
        provider_version: authority.provider_version,
        credential_rotation_version: authority.credential_rotation_version,
        fingerprint: &authority.fingerprint,
    }
}

fn command_receipt(
    authority: &FrozenModelRouteAuthority,
    request_id: &RequestId,
    digest_body: &CommandDigest<'_>,
) -> Result<CommandReceipt, ModelAdmissionError> {
    validate_request_id(request_id)?;
    let actor = ReceiptActorKey::from_encoded(b"winwincode.model-admission.actor.v1".to_vec())?;
    let scope = ReceiptScopeKey::from_encoded(
        route_scope_digest(b"winwincode.model-admission-receipt-scope.v1\0", authority)?.to_vec(),
    )?;
    let identity = ReceiptIdentity::new(actor, scope, request_id.clone())?;
    let payload = serde_json::to_vec(digest_body).map_err(|_| ModelAdmissionError::invalid())?;
    Ok(CommandReceipt {
        identity,
        digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(payload))),
    })
}

fn stream_id(authority: &FrozenModelRouteAuthority) -> Result<String, ModelAdmissionError> {
    Ok(format!(
        "{STREAM_PREFIX}{:x}",
        Sha256::new()
            .chain_update(route_scope_digest(
                b"winwincode.model-admission-stream.v1\0",
                authority,
            )?)
            .finalize()
    ))
}

fn route_scope_digest(
    domain: &[u8],
    authority: &FrozenModelRouteAuthority,
) -> Result<[u8; 32], ModelAdmissionError> {
    let binding = RouteBinding {
        target: authority.target.clone(),
        route: authority.route.clone(),
    };
    let payload = serde_json::to_vec(&binding).map_err(|_| ModelAdmissionError::invalid())?;
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(payload);
    Ok(digest.finalize().into())
}

fn target_scope(target: &ModelSettingsTarget) -> Result<Scope, ModelAdmissionError> {
    match target {
        ModelSettingsTarget::ProductSession {
            repository_scope, ..
        } => Ok(Scope::RepositoryScope(repository_scope.clone())),
        ModelSettingsTarget::Organization { .. }
        | ModelSettingsTarget::Project { .. }
        | ModelSettingsTarget::Repository { .. } => Err(ModelAdmissionError::identity_mismatch()),
    }
}

fn authority_scope_covers_target(authority: &Scope, target: &Scope) -> bool {
    let Scope::RepositoryScope(target) = target else {
        return false;
    };
    match authority {
        Scope::OrganizationScope(scope) => scope.organization_id == target.organization_id,
        Scope::WorkspaceScope(scope) => {
            scope.organization_id == target.organization_id
                && scope.workspace_id == target.workspace_id
        }
        Scope::ProjectScope(scope) => {
            scope.organization_id == target.organization_id
                && scope.workspace_id == target.workspace_id
                && scope.project_id == target.project_id
        }
        Scope::RepositoryScope(scope) => scope == target,
    }
}

fn next_revision(revision: u64) -> Result<u64, ModelAdmissionError> {
    revision
        .checked_add(1)
        .filter(|next| *next <= MAX_SAFE_INTEGER)
        .ok_or_else(ModelAdmissionError::corrupt)
}

fn checked_add(left: u64, right: u64) -> Result<u64, ModelAdmissionError> {
    left.checked_add(right)
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or_else(ModelAdmissionError::corrupt)
}

fn checked_sub(left: u64, right: u64) -> Result<u64, ModelAdmissionError> {
    left.checked_sub(right)
        .ok_or_else(ModelAdmissionError::corrupt)
}

fn validate_request_id(request_id: &RequestId) -> Result<(), ModelAdmissionError> {
    validate_prefixed_id(&request_id.0, "req_")
}

fn validate_model_exchange_id(
    model_exchange_id: &ModelExchangeId,
) -> Result<(), ModelAdmissionError> {
    validate_prefixed_id(&model_exchange_id.0, "mdl_")
}

fn validate_prefixed_id(value: &str, prefix: &str) -> Result<(), ModelAdmissionError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(ModelAdmissionError::invalid());
    };
    if suffix.len() == 26
        && suffix.bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(
                    byte,
                    b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
                )
        })
    {
        Ok(())
    } else {
        Err(ModelAdmissionError::invalid())
    }
}

fn validate_token(value: &str, max_len: usize) -> Result<(), ModelAdmissionError> {
    if value.is_empty()
        || value.len() > max_len
        || value.chars().any(char::is_control)
        || value.trim() != value
    {
        return Err(ModelAdmissionError::invalid());
    }
    Ok(())
}
