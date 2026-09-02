// SPDX-License-Identifier: Apache-2.0

//! Durable model admission at the Provider invocation boundary.
//!
//! This module is the single production bridge from already-resolved
//! Settings, Catalog, and Credential-reference facts to the durable model
//! admission ledger. It owns no secret material and no organization billing
//! ledger.

use std::{
    fmt,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use winwincode_domain::{ModelExchangeId, RequestId};
use winwincode_execution_port::generated::ModelOpenMessage;
use winwincode_storage::{EnterpriseQuotaAmounts, ProductStateStorage, SqliteStorage};

use crate::{
    CredentialReferenceResolution, FrozenModelRouteAuthority, ModelAdmissionClock,
    ModelAdmissionClockError, ModelAdmissionError, ModelAdmissionErrorKind, ModelAdmissionService,
    ModelPolicyAuthorityPort, ModelPolicyResolutionError, ModelRequestAdmission,
    ModelReservationCompletion, ModelReservationReceipt, ModelReservationRelease,
    ModelReservationReleaseReason, ModelReservationRequest, ModelReservationTerminalReceipt,
    ModelSettingsProjection, ProductionModelPolicySource, ProviderGatewayIdentity,
    ProviderTokenUsage, ResolvedModelCapability,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Conservative operational reservation configured by the deployment.
///
/// These are capacity estimates only. Final Provider usage and cost come from
/// the trusted terminal charge and are owned by the separate Usage ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderAdmissionReservationConfig {
    estimated_tokens: u64,
    estimated_cost_micros: u64,
}

impl ProviderAdmissionReservationConfig {
    /// Creates one bounded deployment reservation estimate.
    ///
    /// # Errors
    ///
    /// Rejects zero/unsafe token estimates or unsafe cost estimates.
    pub fn try_new(
        estimated_tokens: u64,
        estimated_cost_micros: u64,
    ) -> Result<Self, ProviderAdmissionError> {
        if estimated_tokens == 0
            || estimated_tokens > MAX_SAFE_INTEGER
            || estimated_cost_micros > MAX_SAFE_INTEGER
        {
            return Err(ProviderAdmissionError::invalid());
        }
        Ok(Self {
            estimated_tokens,
            estimated_cost_micros,
        })
    }
}

/// Exact secret-free facts resolved by the Gateway for one pre-open attempt.
pub struct ProviderAdmissionOpenRequest<'request> {
    pub identity: &'request ProviderGatewayIdentity,
    pub settings: &'request ModelSettingsProjection,
    pub capability: &'request ResolvedModelCapability,
    pub credential: &'request CredentialReferenceResolution,
    pub message: &'request ModelOpenMessage,
}

impl fmt::Debug for ProviderAdmissionOpenRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAdmissionOpenRequest")
            .field("identity", self.identity)
            .field("settings_revision", &self.settings.revision)
            .field("catalog_version", &self.capability.catalog_version)
            .field("provider_version", &self.capability.provider_version)
            .field(
                "credential_rotation_version",
                &self.credential.rotation_version(),
            )
            .field("model_exchange_id", &self.message.model_exchange_id)
            .field("request_id", &self.message.request_id)
            .finish_non_exhaustive()
    }
}

/// Successful durable pre-open result. A denial is still a durable result and
/// is represented by `reservation.denial`.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderAdmissionOpenReceipt {
    pub route_authority: FrozenModelRouteAuthority,
    pub reservation: ModelReservationReceipt,
    /// Immutable enterprise quota reserve copied from trusted deployment admission facts.
    pub enterprise_quota_amounts: EnterpriseQuotaAmounts,
}

/// Stable Provider-admission bridge failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderAdmissionErrorKind {
    InvalidRequest,
    AuthorityUnavailable,
    IdentityMismatch,
    Conflict,
    ClockUnavailable,
    Storage,
}

/// Bounded error containing no Provider payload, secret, or authority text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderAdmissionError {
    kind: ProviderAdmissionErrorKind,
    message: &'static str,
}

impl ProviderAdmissionError {
    const fn new(kind: ProviderAdmissionErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            ProviderAdmissionErrorKind::InvalidRequest,
            "Provider admission request is invalid",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderAdmissionErrorKind {
        self.kind
    }
}

impl fmt::Display for ProviderAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProviderAdmissionError {}

/// Unique Gateway seam for durable reserve/release/complete operations.
pub trait ProviderGatewayAdmissionPort: Send {
    /// Freezes route/policy authority and durably reserves before Provider use.
    ///
    /// # Errors
    ///
    /// Fails closed for unavailable authority, conflicts, or durable failures.
    fn reserve(
        &mut self,
        request: &ProviderAdmissionOpenRequest<'_>,
    ) -> Result<ProviderAdmissionOpenReceipt, ProviderAdmissionError>;

    /// Releases a cancelled or failed reservation exactly once.
    ///
    /// # Errors
    ///
    /// Rejects changed authority/body replay or unavailable durable storage.
    fn release(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        reason: ModelReservationReleaseReason,
    ) -> Result<ModelReservationTerminalReceipt, ProviderAdmissionError>;

    /// Recovery-only conditional release. Absence means reserve never began.
    ///
    /// # Errors
    ///
    /// Rejects foreign/corrupt authority or unavailable durable storage.
    fn release_if_reserved(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        reason: ModelReservationReleaseReason,
    ) -> Result<Option<ModelReservationTerminalReceipt>, ProviderAdmissionError> {
        self.release(authority, original_request_id, model_exchange_id, reason)
            .map(Some)
    }

    /// Completes a successful reservation with trusted normalized usage/cost.
    ///
    /// # Errors
    ///
    /// Rejects changed authority/body replay or unavailable durable storage.
    fn complete(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        usage: ProviderTokenUsage,
        actual_cost_micros: u64,
    ) -> Result<ModelReservationTerminalReceipt, ProviderAdmissionError>;
}

/// Production UTC minute clock used by the durable admission ledger.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemModelAdmissionClock;

impl ModelAdmissionClock for SystemModelAdmissionClock {
    fn unix_minute(&self) -> Result<u64, ModelAdmissionClockError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() / 60)
            .map_err(|_| ModelAdmissionClockError)
    }
}

/// Production Provider-admission adapter over an independent connection to
/// the canonical Control Plane database.
pub struct DurableProviderGatewayAdmission<'authority, 'clock> {
    storage: SqliteStorage,
    clock: &'clock dyn ModelAdmissionClock,
    policy_source: ProductionModelPolicySource<'authority>,
    reservation: ProviderAdmissionReservationConfig,
}

impl<'authority, 'clock> DurableProviderGatewayAdmission<'authority, 'clock> {
    /// Creates the unique production admission bridge.
    #[must_use]
    pub const fn new(
        storage: SqliteStorage,
        clock: &'clock dyn ModelAdmissionClock,
        policy_authority: &'authority dyn ModelPolicyAuthorityPort,
        reservation: ProviderAdmissionReservationConfig,
    ) -> Self {
        Self {
            storage,
            clock,
            policy_source: ProductionModelPolicySource::new(policy_authority),
            reservation,
        }
    }

    /// Returns the canonical database path for composition-root equality checks.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        self.storage.database_path()
    }

    /// Deterministically checkpoints and closes the admission connection.
    ///
    /// # Errors
    ///
    /// Returns a bounded storage failure.
    pub fn close(self) -> Result<(), ProviderAdmissionError> {
        Box::new(self.storage).close().map_err(|_| {
            ProviderAdmissionError::new(
                ProviderAdmissionErrorKind::Storage,
                "Provider admission storage close failed",
            )
        })
    }
}

impl ProviderGatewayAdmissionPort for DurableProviderGatewayAdmission<'_, '_> {
    fn reserve(
        &mut self,
        request: &ProviderAdmissionOpenRequest<'_>,
    ) -> Result<ProviderAdmissionOpenReceipt, ProviderAdmissionError> {
        let authority = FrozenModelRouteAuthority::from_resolved_authority(
            request.identity,
            request.settings,
            request.capability,
            request.credential,
        )
        .map_err(|error| map_admission_error(&error))?;
        let policy = self
            .policy_source
            .resolve(&authority)
            .map_err(|error| map_policy_error(&error))?;
        let admission = ModelRequestAdmission::from_gateway_route(
            request.identity,
            authority.route(),
            request.message.model_exchange_id.clone(),
            request.message.request_id.clone(),
        )
        .map_err(|_| ProviderAdmissionError::invalid())?;
        let reservation_request = ModelReservationRequest::try_new(
            admission,
            self.reservation.estimated_tokens,
            self.reservation.estimated_cost_micros,
        )
        .map_err(|error| map_admission_error(&error))?;
        let receipt = ModelAdmissionService::new(&mut self.storage, self.clock)
            .reserve(&authority, policy.policy(), &reservation_request)
            .map_err(|error| map_admission_error(&error))?;
        if receipt.route_authority_fingerprint != authority.fingerprint() {
            return Err(ProviderAdmissionError::new(
                ProviderAdmissionErrorKind::IdentityMismatch,
                "Provider admission receipt belongs to another route authority",
            ));
        }
        Ok(ProviderAdmissionOpenReceipt {
            route_authority: authority,
            reservation: receipt,
            enterprise_quota_amounts: EnterpriseQuotaAmounts {
                tokens: self.reservation.estimated_tokens,
                provider_cost_micros: self.reservation.estimated_cost_micros,
                operations: 1,
                ..EnterpriseQuotaAmounts::default()
            },
        })
    }

    fn release(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        reason: ModelReservationReleaseReason,
    ) -> Result<ModelReservationTerminalReceipt, ProviderAdmissionError> {
        let request = ModelReservationRelease {
            request_id: terminal_request_id(
                original_request_id,
                model_exchange_id,
                authority,
                match reason {
                    ModelReservationReleaseReason::Cancelled => b"cancelled",
                    ModelReservationReleaseReason::ProviderFailed => b"provider-failed",
                },
            ),
            model_exchange_id: model_exchange_id.clone(),
            reason,
        };
        ModelAdmissionService::new(&mut self.storage, self.clock)
            .release(authority, &request)
            .map_err(|error| map_admission_error(&error))
    }

    fn release_if_reserved(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        reason: ModelReservationReleaseReason,
    ) -> Result<Option<ModelReservationTerminalReceipt>, ProviderAdmissionError> {
        let request = ModelReservationRelease {
            request_id: terminal_request_id(
                original_request_id,
                model_exchange_id,
                authority,
                match reason {
                    ModelReservationReleaseReason::Cancelled => b"cancelled",
                    ModelReservationReleaseReason::ProviderFailed => b"provider-failed",
                },
            ),
            model_exchange_id: model_exchange_id.clone(),
            reason,
        };
        ModelAdmissionService::new(&mut self.storage, self.clock)
            .release_if_reserved(authority, &request)
            .map_err(|error| map_admission_error(&error))
    }

    fn complete(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        usage: ProviderTokenUsage,
        actual_cost_micros: u64,
    ) -> Result<ModelReservationTerminalReceipt, ProviderAdmissionError> {
        let request = ModelReservationCompletion {
            request_id: terminal_request_id(
                original_request_id,
                model_exchange_id,
                authority,
                b"completed",
            ),
            model_exchange_id: model_exchange_id.clone(),
            usage,
            actual_cost_micros,
        };
        ModelAdmissionService::new(&mut self.storage, self.clock)
            .complete(authority, &request)
            .map_err(|error| map_admission_error(&error))
    }
}

impl fmt::Debug for DurableProviderGatewayAdmission<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableProviderGatewayAdmission")
            .field("database_path", &"<redacted>")
            .field("reservation", &self.reservation)
            .finish_non_exhaustive()
    }
}

fn terminal_request_id(
    original_request_id: &RequestId,
    model_exchange_id: &ModelExchangeId,
    authority: &FrozenModelRouteAuthority,
    terminal_kind: &[u8],
) -> RequestId {
    let mut hash = Sha256::new();
    hash.update(b"winwincode.provider-admission-terminal.v1");
    hash.update(original_request_id.0.as_bytes());
    hash.update(model_exchange_id.0.as_bytes());
    hash.update(authority.fingerprint().as_bytes());
    hash.update(terminal_kind);
    let digest = hash.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    let mut value = u128::from_be_bytes(bytes);
    let mut suffix = [b'0'; 26];
    for byte in suffix.iter_mut().rev() {
        *byte = CROCKFORD_BASE32[(value & 31) as usize];
        value >>= 5;
    }
    RequestId(format!(
        "req_{}",
        String::from_utf8(suffix.to_vec()).expect("Crockford base32 is ASCII")
    ))
}

fn map_policy_error(_error: &ModelPolicyResolutionError) -> ProviderAdmissionError {
    ProviderAdmissionError::new(
        ProviderAdmissionErrorKind::AuthorityUnavailable,
        "Provider policy authority is unavailable",
    )
}

fn map_admission_error(error: &ModelAdmissionError) -> ProviderAdmissionError {
    let (kind, message) = match error.kind() {
        ModelAdmissionErrorKind::InvalidRequest => (
            ProviderAdmissionErrorKind::InvalidRequest,
            "Provider admission request is invalid",
        ),
        ModelAdmissionErrorKind::IdentityMismatch => (
            ProviderAdmissionErrorKind::IdentityMismatch,
            "Provider admission route authority does not match",
        ),
        ModelAdmissionErrorKind::ReservationConflict
        | ModelAdmissionErrorKind::ReservationNotFound
        | ModelAdmissionErrorKind::TerminalConflict
        | ModelAdmissionErrorKind::RequestConflict => (
            ProviderAdmissionErrorKind::Conflict,
            "Provider admission replay conflicts with durable state",
        ),
        ModelAdmissionErrorKind::ClockUnavailable => (
            ProviderAdmissionErrorKind::ClockUnavailable,
            "Provider admission clock is unavailable",
        ),
        ModelAdmissionErrorKind::CorruptState | ModelAdmissionErrorKind::Storage => (
            ProviderAdmissionErrorKind::Storage,
            "Provider admission storage operation failed",
        ),
    };
    ProviderAdmissionError::new(kind, message)
}
