// SPDX-License-Identifier: Apache-2.0

//! Enterprise allowance admission backed by the canonical Usage and quota ledgers.
//!
//! The permit returned here is deliberately separate from Provider capacity
//! and scheduler-slot admission. Callers must obtain both authorities.

use std::path::Path;

use winwincode_storage::{
    EnterpriseQuotaDecision, EnterpriseQuotaDenial, EnterpriseQuotaError, EnterpriseQuotaPolicy,
    EnterpriseQuotaPolicyReceipt, EnterpriseQuotaRelease, EnterpriseQuotaReservationReceipt,
    EnterpriseQuotaReservationRequest, EnterpriseQuotaSettlement, ProductStateStorage,
    SqliteStorage, StorageError,
};

/// Opaque proof that every configured enterprise boundary admitted a request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseQuotaPermit {
    receipt: EnterpriseQuotaReservationReceipt,
}

impl EnterpriseQuotaPermit {
    /// Returns the durable reservation receipt used for downstream binding.
    #[must_use]
    pub const fn receipt(&self) -> &EnterpriseQuotaReservationReceipt {
        &self.receipt
    }
}

/// Result at the enterprise admission boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnterpriseQuotaAdmission {
    Admitted(Box<EnterpriseQuotaPermit>),
    TerminalReplay(Box<EnterpriseQuotaReservationReceipt>),
    Denied(EnterpriseQuotaDenial),
}

/// Unique enterprise quota seam used before operational admission.
pub trait EnterpriseQuotaAdmissionPort: Send {
    /// Reserves allowance or returns the exact enterprise denial.
    ///
    /// # Errors
    ///
    /// Fails closed for malformed authority, replay conflicts, corruption, or
    /// unavailable durable storage.
    fn reserve(
        &mut self,
        request: &EnterpriseQuotaReservationRequest,
    ) -> Result<EnterpriseQuotaAdmission, EnterpriseQuotaError>;

    /// Releases allowance after cancellation, failure, or downstream denial.
    ///
    /// # Errors
    ///
    /// Rejects stale revisions, changed replay, corruption, or unavailable storage.
    fn release(
        &mut self,
        release: &EnterpriseQuotaRelease,
    ) -> Result<EnterpriseQuotaReservationReceipt, EnterpriseQuotaError>;

    /// Settles allowance only from one immutable Usage ledger source.
    ///
    /// # Errors
    ///
    /// Rejects missing, foreign, stale, over-reserved, or corrupt settlement facts.
    fn settle(
        &mut self,
        settlement: &EnterpriseQuotaSettlement,
    ) -> Result<EnterpriseQuotaReservationReceipt, EnterpriseQuotaError>;

    /// Recovers and settles the reservation sealed to one immutable Usage source.
    ///
    /// Sources without a matching reservation return `None`; matching released
    /// or corrupt reservations fail closed.
    ///
    /// # Errors
    ///
    /// Rejects a missing/corrupt Usage source, changed authority, or unavailable storage.
    fn settle_usage_source(
        &mut self,
        source: &winwincode_storage::EnterpriseUsageSource,
    ) -> Result<Option<EnterpriseQuotaReservationReceipt>, EnterpriseQuotaError>;
}

/// Production enterprise quota port over an independent connection to the
/// canonical Control Plane database.
pub struct DurableEnterpriseQuotaAdmission {
    storage: SqliteStorage,
}

impl DurableEnterpriseQuotaAdmission {
    /// Creates the unique durable enterprise quota admission port.
    #[must_use]
    pub const fn new(storage: SqliteStorage) -> Self {
        Self { storage }
    }

    /// Returns the canonical database path for composition-root equality checks.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        self.storage.database_path()
    }

    /// Creates the next immutable quota policy revision.
    ///
    /// # Errors
    ///
    /// Rejects malformed policies, revision gaps, changed replay, corruption,
    /// or unavailable durable storage.
    pub fn put_policy(
        &mut self,
        policy: &EnterpriseQuotaPolicy,
    ) -> Result<EnterpriseQuotaPolicyReceipt, EnterpriseQuotaError> {
        self.storage.enterprise_quota_ledger()?.put_policy(policy)
    }

    /// Loads the current immutable quota policy for one boundary.
    ///
    /// # Errors
    ///
    /// Rejects malformed boundaries, corruption, or unavailable durable storage.
    pub fn load_policy(
        &mut self,
        boundary: &winwincode_storage::EnterpriseQuotaBoundary,
    ) -> Result<Option<EnterpriseQuotaPolicyReceipt>, EnterpriseQuotaError> {
        self.storage
            .enterprise_quota_ledger()?
            .load_policy(boundary)
    }

    /// Deterministically checkpoints and closes the quota connection.
    ///
    /// # Errors
    ///
    /// Returns a bounded storage failure.
    pub fn close(self) -> Result<(), StorageError> {
        Box::new(self.storage).close()
    }
}

impl EnterpriseQuotaAdmissionPort for DurableEnterpriseQuotaAdmission {
    fn reserve(
        &mut self,
        request: &EnterpriseQuotaReservationRequest,
    ) -> Result<EnterpriseQuotaAdmission, EnterpriseQuotaError> {
        self.storage
            .enterprise_quota_ledger()?
            .reserve(request)
            .map(|decision| match decision {
                EnterpriseQuotaDecision::Allowed(receipt) => {
                    EnterpriseQuotaAdmission::Admitted(Box::new(EnterpriseQuotaPermit {
                        receipt: *receipt,
                    }))
                }
                EnterpriseQuotaDecision::TerminalReplay(receipt) => {
                    EnterpriseQuotaAdmission::TerminalReplay(receipt)
                }
                EnterpriseQuotaDecision::Denied(denial) => EnterpriseQuotaAdmission::Denied(denial),
            })
    }

    fn release(
        &mut self,
        release: &EnterpriseQuotaRelease,
    ) -> Result<EnterpriseQuotaReservationReceipt, EnterpriseQuotaError> {
        self.storage.enterprise_quota_ledger()?.release(release)
    }

    fn settle(
        &mut self,
        settlement: &EnterpriseQuotaSettlement,
    ) -> Result<EnterpriseQuotaReservationReceipt, EnterpriseQuotaError> {
        self.storage.enterprise_quota_ledger()?.settle(settlement)
    }

    fn settle_usage_source(
        &mut self,
        source: &winwincode_storage::EnterpriseUsageSource,
    ) -> Result<Option<EnterpriseQuotaReservationReceipt>, EnterpriseQuotaError> {
        self.storage
            .enterprise_quota_ledger()?
            .settle_usage_source(source)
    }
}
