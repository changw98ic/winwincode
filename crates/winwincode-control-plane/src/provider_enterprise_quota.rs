// SPDX-License-Identifier: Apache-2.0

//! Provider enterprise-quota ordering around the durable retry context.
//!
//! This module deliberately owns no Provider request pool, adapter, or retry
//! state. It turns an already-persisted [`ModelRetrySettlementContext`] into
//! the only enterprise reservation authority, orders it before the existing
//! operational admission, and settles it only from the immutable enterprise
//! Provider Usage entry.

use std::{fmt, path::Path};

use sha2::{Digest, Sha256};
use winwincode_domain::{Instant, ModelExchangeId, RequestId};
use winwincode_storage::{
    EnterpriseQuotaAmounts, EnterpriseQuotaError, EnterpriseQuotaRelease,
    EnterpriseQuotaReleaseReason, EnterpriseQuotaReservationReceipt,
    EnterpriseQuotaReservationRequest, EnterpriseQuotaSourceSeal, EnterpriseUsageAttribution,
    EnterpriseUsageEntry, EnterpriseUsageError, EnterpriseUsageSource, ProductStateStorage,
    SqliteStorage, StorageError,
};

use crate::{
    EnterpriseQuotaAdmission, EnterpriseQuotaAdmissionPort, EnterpriseQuotaPermit,
    ModelRetrySettlementContext, ModelRetrySettlementContextErrorKind,
    ModelRetrySettlementContextPort,
};

/// Stable failure categories for Provider enterprise quota orchestration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderEnterpriseQuotaErrorKind {
    ContextMissing,
    ContextCorrupt,
    ContextUnavailable,
    Quota,
    UsageSourceUnavailable,
    UsageSourceMissing,
    UsageSourceMismatch,
    OperationalAdmission,
    Rollback,
}

/// Secret-free Provider enterprise quota orchestration error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderEnterpriseQuotaError {
    kind: ProviderEnterpriseQuotaErrorKind,
}

impl ProviderEnterpriseQuotaError {
    const fn new(kind: ProviderEnterpriseQuotaErrorKind) -> Self {
        Self { kind }
    }

    /// Returns the stable machine-readable failure category.
    #[must_use]
    pub const fn kind(&self) -> ProviderEnterpriseQuotaErrorKind {
        self.kind
    }
}

impl fmt::Display for ProviderEnterpriseQuotaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Provider enterprise quota operation failed")
    }
}

impl std::error::Error for ProviderEnterpriseQuotaError {}

/// Exact operational admission result used only to sequence existing capacity authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderOperationalAdmissionError {
    Denied,
    Unavailable,
}

/// Existing Provider operational admission invoked after enterprise allowance.
///
/// Implementations must delegate to the existing Provider admission authority;
/// they must not implement a second request pool or capacity counter.
pub trait ProviderOperationalAdmissionPort {
    type Receipt;

    /// Reserves existing operational capacity for the already-authorized exchange.
    ///
    /// # Errors
    ///
    /// Returns the existing authority's stable denial or availability category.
    fn reserve(&mut self) -> Result<Self::Receipt, ProviderOperationalAdmissionError>;
}

/// Result of sequencing enterprise and operational Provider admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderEnterpriseQuotaOpen<Receipt> {
    Admitted {
        reservation: Box<ProviderEnterpriseQuotaReservation>,
        operational: Receipt,
    },
    TerminalReplay(Box<EnterpriseQuotaReservationReceipt>),
    Denied,
}

/// Active Provider enterprise reservation bound to one durable retry context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderEnterpriseQuotaReservation {
    request: EnterpriseQuotaReservationRequest,
    permit: EnterpriseQuotaPermit,
}

impl ProviderEnterpriseQuotaReservation {
    /// Returns the canonical quota request derived from durable authority.
    #[must_use]
    pub const fn request(&self) -> &EnterpriseQuotaReservationRequest {
        &self.request
    }

    /// Returns the opaque enterprise permit.
    #[must_use]
    pub const fn permit(&self) -> &EnterpriseQuotaPermit {
        &self.permit
    }
}

/// Immutable Enterprise Usage source reader used for terminal Provider settlement.
pub trait ProviderEnterpriseUsageSourcePort {
    /// Loads one source entry from the immutable Enterprise Usage ledger.
    ///
    /// A missing entry is a fail-closed fact: projection must complete before
    /// the quota reservation may be settled.
    ///
    /// # Errors
    ///
    /// Returns a bounded unavailable or source-mismatch category.
    fn load_source(
        &mut self,
        source: &EnterpriseUsageSource,
    ) -> Result<Option<EnterpriseUsageEntry>, ProviderEnterpriseQuotaError>;
}

/// Production reader over the canonical Enterprise Usage ledger.
pub struct DurableProviderEnterpriseUsageSource {
    storage: SqliteStorage,
}

impl DurableProviderEnterpriseUsageSource {
    /// Opens the canonical Enterprise Usage ledger reader.
    #[must_use]
    pub const fn new(storage: SqliteStorage) -> Self {
        Self { storage }
    }

    /// Returns the canonical database path for composition checks.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        self.storage.database_path()
    }

    /// Deterministically checkpoints and closes the reader connection.
    ///
    /// # Errors
    ///
    /// Returns a bounded storage error when closing fails.
    pub fn close(self) -> Result<(), StorageError> {
        Box::new(self.storage).close()
    }
}

impl ProviderEnterpriseUsageSourcePort for DurableProviderEnterpriseUsageSource {
    fn load_source(
        &mut self,
        source: &EnterpriseUsageSource,
    ) -> Result<Option<EnterpriseUsageEntry>, ProviderEnterpriseQuotaError> {
        match source {
            EnterpriseUsageSource::Provider { .. } => self
                .storage
                .enterprise_usage_ledger()
                .map_err(map_usage_error)?
                .load_source(source)
                .map_err(map_usage_error),
            EnterpriseUsageSource::Worker { .. }
            | EnterpriseUsageSource::Storage { .. }
            | EnterpriseUsageSource::Publication { .. } => Err(ProviderEnterpriseQuotaError::new(
                ProviderEnterpriseQuotaErrorKind::UsageSourceMismatch,
            )),
        }
    }
}

/// Provider quota orchestrator using the one enterprise quota admission port.
pub struct ProviderEnterpriseQuotaSaga<'quota> {
    quota: &'quota mut dyn EnterpriseQuotaAdmissionPort,
}

impl<'quota> ProviderEnterpriseQuotaSaga<'quota> {
    /// Creates an orchestrator over the unique enterprise quota admission port.
    #[must_use]
    pub fn new(quota: &'quota mut dyn EnterpriseQuotaAdmissionPort) -> Self {
        Self { quota }
    }

    /// Reserves enterprise allowance from a durable retry context, then invokes
    /// the existing operational admission exactly once.
    ///
    /// Enterprise denial and terminal replay do not invoke `operational`. An
    /// operational failure releases the active enterprise reservation before it
    /// is returned to the caller.
    ///
    /// # Errors
    ///
    /// Fails closed for missing/corrupt durable context, quota storage failures,
    /// operational admission failure, or failed rollback.
    pub fn reserve_then_admit<Operational>(
        &mut self,
        contexts: &dyn ModelRetrySettlementContextPort,
        model_exchange_id: &ModelExchangeId,
        amounts: EnterpriseQuotaAmounts,
        requested_at: Instant,
        operational: &mut Operational,
    ) -> Result<ProviderEnterpriseQuotaOpen<Operational::Receipt>, ProviderEnterpriseQuotaError>
    where
        Operational: ProviderOperationalAdmissionPort,
    {
        let context = load_context(contexts, model_exchange_id)?;
        self.reserve_request_then_admit(
            request_from_context(&context, amounts, requested_at),
            operational,
        )
    }

    /// Reserves the exact request embedded in durable retry context before
    /// delegating to the existing operational Provider admission.
    ///
    /// # Errors
    ///
    /// Fails closed when the context is absent or changed, enterprise quota
    /// rejects the request, or operational admission cannot be completed.
    pub fn reserve_durable_then_admit<Operational>(
        &mut self,
        contexts: &dyn ModelRetrySettlementContextPort,
        model_exchange_id: &ModelExchangeId,
        operational: &mut Operational,
    ) -> Result<ProviderEnterpriseQuotaOpen<Operational::Receipt>, ProviderEnterpriseQuotaError>
    where
        Operational: ProviderOperationalAdmissionPort,
    {
        let context = load_context(contexts, model_exchange_id)?;
        self.reserve_request_then_admit(context.enterprise_quota_request().clone(), operational)
    }

    fn reserve_request_then_admit<Operational>(
        &mut self,
        request: EnterpriseQuotaReservationRequest,
        operational: &mut Operational,
    ) -> Result<ProviderEnterpriseQuotaOpen<Operational::Receipt>, ProviderEnterpriseQuotaError>
    where
        Operational: ProviderOperationalAdmissionPort,
    {
        match self
            .quota
            .reserve(&request)
            .map_err(|error| map_quota_error(&error))?
        {
            EnterpriseQuotaAdmission::Admitted(permit) => {
                if let Ok(receipt) = operational.reserve() {
                    Ok(ProviderEnterpriseQuotaOpen::Admitted {
                        reservation: Box::new(ProviderEnterpriseQuotaReservation {
                            request,
                            permit: *permit,
                        }),
                        operational: receipt,
                    })
                } else {
                    let release = release_request(
                        &request,
                        permit.receipt().record.revision,
                        EnterpriseQuotaReleaseReason::OperationalAdmissionDenied,
                        request.requested_at.clone(),
                    );
                    self.quota.release(&release).map_err(|_| {
                        ProviderEnterpriseQuotaError::new(
                            ProviderEnterpriseQuotaErrorKind::Rollback,
                        )
                    })?;
                    Err(ProviderEnterpriseQuotaError::new(
                        ProviderEnterpriseQuotaErrorKind::OperationalAdmission,
                    ))
                }
            }
            EnterpriseQuotaAdmission::TerminalReplay(receipt) => {
                Ok(ProviderEnterpriseQuotaOpen::TerminalReplay(receipt))
            }
            EnterpriseQuotaAdmission::Denied(_) => Ok(ProviderEnterpriseQuotaOpen::Denied),
        }
    }

    /// Releases an active Provider reservation after a terminal failure or cancellation.
    ///
    /// # Errors
    ///
    /// Returns a bounded quota failure when the exact terminal mutation cannot
    /// be written or replayed.
    pub fn release(
        &mut self,
        reservation: &ProviderEnterpriseQuotaReservation,
        reason: EnterpriseQuotaReleaseReason,
        released_at: Instant,
    ) -> Result<EnterpriseQuotaReservationReceipt, ProviderEnterpriseQuotaError> {
        self.quota
            .release(&release_request(
                &reservation.request,
                reservation.permit.receipt().record.revision,
                reason,
                released_at,
            ))
            .map_err(|error| map_quota_error(&error))
    }

    /// Replays the durable reservation request and releases it for a failed or
    /// cancelled Provider terminal. Reacquiring the same request is idempotent
    /// and recovers the exact active revision after a restart.
    ///
    /// # Errors
    ///
    /// Fails closed for a missing or changed context, quota failure, denial, or
    /// a terminal state that conflicts with the requested release.
    pub fn release_durable_terminal(
        &mut self,
        contexts: &dyn ModelRetrySettlementContextPort,
        model_exchange_id: &ModelExchangeId,
        reason: EnterpriseQuotaReleaseReason,
        released_at: Instant,
    ) -> Result<EnterpriseQuotaReservationReceipt, ProviderEnterpriseQuotaError> {
        let context = load_context(contexts, model_exchange_id)?;
        let request = context.enterprise_quota_request();
        match self
            .quota
            .reserve(request)
            .map_err(|error| map_quota_error(&error))?
        {
            EnterpriseQuotaAdmission::Admitted(permit) => self
                .quota
                .release(&release_request(
                    request,
                    permit.receipt().record.revision,
                    reason,
                    released_at,
                ))
                .map_err(|error| map_quota_error(&error)),
            EnterpriseQuotaAdmission::TerminalReplay(receipt) => Ok(*receipt),
            EnterpriseQuotaAdmission::Denied(_) => Err(ProviderEnterpriseQuotaError::new(
                ProviderEnterpriseQuotaErrorKind::Quota,
            )),
        }
    }

    /// Settles only from an immutable Provider entry already projected into the
    /// Enterprise Usage ledger. Caller-supplied attribution and raw terminal
    /// token facts are never accepted here.
    ///
    /// # Errors
    ///
    /// Fails closed if the context, quota request, source identity, immutable
    /// Usage projection, or terminal settlement is unavailable or changed.
    pub fn settle_from_usage_source(
        &mut self,
        contexts: &dyn ModelRetrySettlementContextPort,
        reservation: &ProviderEnterpriseQuotaReservation,
        source_reader: &mut dyn ProviderEnterpriseUsageSourcePort,
        source: &EnterpriseUsageSource,
    ) -> Result<EnterpriseQuotaReservationReceipt, ProviderEnterpriseQuotaError> {
        let context = load_context(contexts, &reservation.request.source_model_exchange_id())?;
        if request_from_context(
            &context,
            reservation.request.reserved,
            reservation.request.requested_at.clone(),
        ) != reservation.request
        {
            return Err(ProviderEnterpriseQuotaError::new(
                ProviderEnterpriseQuotaErrorKind::UsageSourceMismatch,
            ));
        }
        let entry = source_reader.load_source(source)?.ok_or_else(|| {
            ProviderEnterpriseQuotaError::new(ProviderEnterpriseQuotaErrorKind::UsageSourceMissing)
        })?;
        if entry.fact.source != *source || !source_matches_context(&context, &entry) {
            return Err(ProviderEnterpriseQuotaError::new(
                ProviderEnterpriseQuotaErrorKind::UsageSourceMismatch,
            ));
        }
        self.quota
            .settle_usage_source(&entry.fact.source)
            .map_err(|error| map_quota_error(&error))?
            .ok_or_else(|| {
                ProviderEnterpriseQuotaError::new(
                    ProviderEnterpriseQuotaErrorKind::UsageSourceMismatch,
                )
            })
    }
}

trait ProviderReservationRequestExt {
    fn source_model_exchange_id(&self) -> ModelExchangeId;
}

impl ProviderReservationRequestExt for EnterpriseQuotaReservationRequest {
    fn source_model_exchange_id(&self) -> ModelExchangeId {
        match &self.source_seal {
            EnterpriseQuotaSourceSeal::Provider {
                model_exchange_id, ..
            } => model_exchange_id.clone(),
            EnterpriseQuotaSourceSeal::Worker { .. }
            | EnterpriseQuotaSourceSeal::Storage { .. }
            | EnterpriseQuotaSourceSeal::Publication { .. } => {
                unreachable!("Provider quota reservations always have a Provider source seal")
            }
        }
    }
}

fn load_context(
    contexts: &dyn ModelRetrySettlementContextPort,
    model_exchange_id: &ModelExchangeId,
) -> Result<ModelRetrySettlementContext, ProviderEnterpriseQuotaError> {
    contexts
        .load_context(model_exchange_id)
        .map_err(|error| match error.kind() {
            ModelRetrySettlementContextErrorKind::Corrupt => {
                ProviderEnterpriseQuotaError::new(ProviderEnterpriseQuotaErrorKind::ContextCorrupt)
            }
            ModelRetrySettlementContextErrorKind::Unavailable => ProviderEnterpriseQuotaError::new(
                ProviderEnterpriseQuotaErrorKind::ContextUnavailable,
            ),
        })?
        .ok_or_else(|| {
            ProviderEnterpriseQuotaError::new(ProviderEnterpriseQuotaErrorKind::ContextMissing)
        })
}

fn request_from_context(
    context: &ModelRetrySettlementContext,
    reserved: EnterpriseQuotaAmounts,
    requested_at: Instant,
) -> EnterpriseQuotaReservationRequest {
    let start = context.start_receipt();
    EnterpriseQuotaReservationRequest {
        reservation_id: start.reservation_request_id.clone(),
        attribution: EnterpriseUsageAttribution {
            organization_id: context.request().attribution.organization_id.clone(),
            workspace_id: context.request().attribution.workspace_id.clone(),
            project_id: context.request().attribution.project_id.clone(),
            repository_id: context.request().attribution.repository_id.clone(),
            delivery_id: context.request().attribution.delivery_id.clone(),
            product_session_id: Some(context.request().attribution.product_session_id.clone()),
            user_id: context.request().attribution.user_id.clone(),
        },
        source_seal: EnterpriseQuotaSourceSeal::Provider {
            model_exchange_id: start.model_exchange_id.clone(),
            request_id: context.request().request_id.clone(),
            attempt: start.attempt,
            route_authority_fingerprint: start.route_fingerprint.clone(),
        },
        reserved,
        requested_at,
    }
}

fn release_request(
    request: &EnterpriseQuotaReservationRequest,
    expected_revision: u64,
    reason: EnterpriseQuotaReleaseReason,
    released_at: Instant,
) -> EnterpriseQuotaRelease {
    EnterpriseQuotaRelease {
        reservation_id: request.reservation_id.clone(),
        request_id: terminal_request_id(&request.reservation_id, "release"),
        expected_revision,
        reason,
        released_at,
    }
}

fn source_matches_context(
    context: &ModelRetrySettlementContext,
    entry: &EnterpriseUsageEntry,
) -> bool {
    let start = context.start_receipt();
    let attribution = &context.request().attribution;
    let attribution_matches = entry.fact.attribution.organization_id == attribution.organization_id
        && entry.fact.attribution.workspace_id == attribution.workspace_id
        && entry.fact.attribution.project_id == attribution.project_id
        && entry.fact.attribution.repository_id == attribution.repository_id
        && entry.fact.attribution.delivery_id == attribution.delivery_id
        && entry.fact.attribution.product_session_id.as_ref()
            == Some(&attribution.product_session_id)
        && entry.fact.attribution.user_id == attribution.user_id;
    let source_matches = matches!(
        &entry.fact.source,
        EnterpriseUsageSource::Provider {
            model_exchange_id,
            request_id,
            attempt,
            route_authority_fingerprint,
            ..
        } if model_exchange_id == &start.model_exchange_id
            && request_id == &context.request().request_id
            && *attempt == start.attempt
            && route_authority_fingerprint == &start.route_fingerprint
    );
    attribution_matches && source_matches
}

fn terminal_request_id(reservation_id: &RequestId, operation: &str) -> RequestId {
    let digest = Sha256::digest(
        [
            b"winwincode.provider-enterprise-quota-terminal.v1\0".as_slice(),
            operation.as_bytes(),
            b"\0".as_slice(),
            reservation_id.0.as_bytes(),
        ]
        .concat(),
    );
    let mut value = u128::from_be_bytes(digest[..16].try_into().expect("digest prefix fits"));
    let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut suffix = [b'0'; 26];
    for byte in suffix.iter_mut().rev() {
        *byte = alphabet[usize::try_from(value & 31).expect("base32 digit fits usize")];
        value >>= 5;
    }
    RequestId(format!(
        "req_{}",
        std::str::from_utf8(&suffix).expect("Crockford alphabet is UTF-8")
    ))
}

fn map_quota_error(_error: &EnterpriseQuotaError) -> ProviderEnterpriseQuotaError {
    ProviderEnterpriseQuotaError::new(ProviderEnterpriseQuotaErrorKind::Quota)
}

fn map_usage_error(_error: EnterpriseUsageError) -> ProviderEnterpriseQuotaError {
    ProviderEnterpriseQuotaError::new(ProviderEnterpriseQuotaErrorKind::UsageSourceUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_storage::EnterpriseQuotaSettlement;
    struct MissingContext;

    impl ModelRetrySettlementContextPort for MissingContext {
        fn load_context(
            &self,
            _model_exchange_id: &ModelExchangeId,
        ) -> Result<Option<ModelRetrySettlementContext>, crate::ModelRetrySettlementContextError>
        {
            Ok(None)
        }
    }

    struct CorruptContext;

    impl ModelRetrySettlementContextPort for CorruptContext {
        fn load_context(
            &self,
            _model_exchange_id: &ModelExchangeId,
        ) -> Result<Option<ModelRetrySettlementContext>, crate::ModelRetrySettlementContextError>
        {
            Err(crate::ModelRetrySettlementContextError::corrupt())
        }
    }

    struct NeverQuota;

    impl EnterpriseQuotaAdmissionPort for NeverQuota {
        fn reserve(
            &mut self,
            _request: &EnterpriseQuotaReservationRequest,
        ) -> Result<EnterpriseQuotaAdmission, EnterpriseQuotaError> {
            panic!("quota must not run without durable context")
        }

        fn release(
            &mut self,
            _release: &EnterpriseQuotaRelease,
        ) -> Result<EnterpriseQuotaReservationReceipt, EnterpriseQuotaError> {
            panic!("quota release must not run without durable context")
        }

        fn settle(
            &mut self,
            _settlement: &EnterpriseQuotaSettlement,
        ) -> Result<EnterpriseQuotaReservationReceipt, EnterpriseQuotaError> {
            panic!("quota settlement must not run without durable context")
        }

        fn settle_usage_source(
            &mut self,
            _source: &EnterpriseUsageSource,
        ) -> Result<Option<EnterpriseQuotaReservationReceipt>, EnterpriseQuotaError> {
            panic!("quota recovery settlement must not run without durable context")
        }
    }

    struct NeverOperational;

    impl ProviderOperationalAdmissionPort for NeverOperational {
        type Receipt = ();

        fn reserve(&mut self) -> Result<Self::Receipt, ProviderOperationalAdmissionError> {
            panic!("operational admission must not run without enterprise authority")
        }
    }

    fn exchange() -> ModelExchangeId {
        ModelExchangeId("mdl_00000000000000000000000001".to_owned())
    }

    fn amounts() -> EnterpriseQuotaAmounts {
        EnterpriseQuotaAmounts {
            operations: 1,
            ..EnterpriseQuotaAmounts::default()
        }
    }

    fn now() -> Instant {
        Instant("2027-08-01T00:00:00.000Z".to_owned())
    }

    #[test]
    fn missing_durable_context_stops_before_quota_or_operational_admission() {
        let mut quota = NeverQuota;
        let mut operational = NeverOperational;
        let error = ProviderEnterpriseQuotaSaga::new(&mut quota)
            .reserve_then_admit(
                &MissingContext,
                &exchange(),
                amounts(),
                now(),
                &mut operational,
            )
            .expect_err("missing context must fail closed");
        assert_eq!(
            error.kind(),
            ProviderEnterpriseQuotaErrorKind::ContextMissing
        );
    }

    #[test]
    fn corrupt_durable_context_stops_before_quota_or_operational_admission() {
        let mut quota = NeverQuota;
        let mut operational = NeverOperational;
        let error = ProviderEnterpriseQuotaSaga::new(&mut quota)
            .reserve_then_admit(
                &CorruptContext,
                &exchange(),
                amounts(),
                now(),
                &mut operational,
            )
            .expect_err("corrupt context must fail closed");
        assert_eq!(
            error.kind(),
            ProviderEnterpriseQuotaErrorKind::ContextCorrupt
        );
    }
}
