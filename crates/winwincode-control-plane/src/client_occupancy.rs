// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use winwincode_domain::Instant;
use winwincode_storage::{
    OccupancyClaim, OccupancyLeaseRecord, OccupancyReconcileTarget, OccupancyReleaseReason,
    OccupancyStoreError, OccupancyStoreErrorKind, SqliteStorage,
};

pub use winwincode_storage::OccupancyLeaseState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientOccupancyServiceErrorKind {
    InvalidInput,
    UnknownClientNode,
    UnknownOccupancyLease,
    AccessDenied,
    PresenceNotOnline,
    ClientLocked,
    NotAcceptingConnections,
    CapacityExhausted,
    ActiveLeaseConflict,
    OccupancyLeaseConflict,
    FencingTokenMismatch,
    FencingTokenExhausted,
    IllegalStateTransition,
    RevisionConflict,
    CorruptState,
    Storage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientOccupancyServiceError {
    kind: ClientOccupancyServiceErrorKind,
    message: String,
}

impl ClientOccupancyServiceError {
    #[must_use]
    pub const fn kind(&self) -> ClientOccupancyServiceErrorKind {
        self.kind
    }
}

impl fmt::Display for ClientOccupancyServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientOccupancyServiceError {}

impl From<OccupancyStoreError> for ClientOccupancyServiceError {
    fn from(source: OccupancyStoreError) -> Self {
        Self {
            kind: match source.kind() {
                OccupancyStoreErrorKind::InvalidInput => {
                    ClientOccupancyServiceErrorKind::InvalidInput
                }
                OccupancyStoreErrorKind::UnknownClientNode => {
                    ClientOccupancyServiceErrorKind::UnknownClientNode
                }
                OccupancyStoreErrorKind::UnknownOccupancyLease => {
                    ClientOccupancyServiceErrorKind::UnknownOccupancyLease
                }
                OccupancyStoreErrorKind::AccessDenied => {
                    ClientOccupancyServiceErrorKind::AccessDenied
                }
                OccupancyStoreErrorKind::PresenceNotOnline => {
                    ClientOccupancyServiceErrorKind::PresenceNotOnline
                }
                OccupancyStoreErrorKind::ClientLocked => {
                    ClientOccupancyServiceErrorKind::ClientLocked
                }
                OccupancyStoreErrorKind::NotAcceptingConnections => {
                    ClientOccupancyServiceErrorKind::NotAcceptingConnections
                }
                OccupancyStoreErrorKind::CapacityExhausted => {
                    ClientOccupancyServiceErrorKind::CapacityExhausted
                }
                OccupancyStoreErrorKind::ActiveLeaseConflict => {
                    ClientOccupancyServiceErrorKind::ActiveLeaseConflict
                }
                OccupancyStoreErrorKind::OccupancyLeaseConflict => {
                    ClientOccupancyServiceErrorKind::OccupancyLeaseConflict
                }
                OccupancyStoreErrorKind::FencingTokenMismatch => {
                    ClientOccupancyServiceErrorKind::FencingTokenMismatch
                }
                OccupancyStoreErrorKind::FencingTokenExhausted => {
                    ClientOccupancyServiceErrorKind::FencingTokenExhausted
                }
                OccupancyStoreErrorKind::IllegalStateTransition => {
                    ClientOccupancyServiceErrorKind::IllegalStateTransition
                }
                OccupancyStoreErrorKind::RevisionConflict => {
                    ClientOccupancyServiceErrorKind::RevisionConflict
                }
                OccupancyStoreErrorKind::CorruptState => {
                    ClientOccupancyServiceErrorKind::CorruptState
                }
                OccupancyStoreErrorKind::Storage => ClientOccupancyServiceErrorKind::Storage,
            },
            message: source.to_string(),
        }
    }
}

pub struct ClientOccupancyService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> ClientOccupancyService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    pub fn current_fencing_token(&mut self) -> Result<u64, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .current_fencing_token()?)
    }

    pub fn mint_fencing_token(&mut self) -> Result<u64, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .mint_fencing_token()?)
    }

    pub fn atomic_claim(
        &mut self,
        claim: &OccupancyClaim,
        now: &Instant,
    ) -> Result<OccupancyLeaseRecord, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .atomic_claim(claim, now)?)
    }

    pub fn record_acknowledgement(
        &mut self,
        occupancy_lease_id: &str,
        fencing_token: u64,
        idle_expires_at: Option<&Instant>,
        now: &Instant,
    ) -> Result<OccupancyLeaseRecord, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .record_acknowledgement(occupancy_lease_id, fencing_token, idle_expires_at, now)?)
    }

    pub fn reject_offer(
        &mut self,
        occupancy_lease_id: &str,
        fencing_token: u64,
        reason: OccupancyReleaseReason,
        now: &Instant,
    ) -> Result<OccupancyLeaseRecord, ClientOccupancyServiceError> {
        Ok(self.storage.client_occupancy_ledger()?.reject_offer(
            occupancy_lease_id,
            fencing_token,
            reason,
            now,
        )?)
    }

    pub fn request_release(
        &mut self,
        occupancy_lease_id: &str,
        fencing_token: u64,
        active_worker_session_count: u64,
        now: &Instant,
    ) -> Result<OccupancyLeaseRecord, ClientOccupancyServiceError> {
        Ok(self.storage.client_occupancy_ledger()?.request_release(
            occupancy_lease_id,
            fencing_token,
            active_worker_session_count,
            now,
        )?)
    }

    pub fn drain_complete(
        &mut self,
        occupancy_lease_id: &str,
    ) -> Result<OccupancyLeaseRecord, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .drain_complete(occupancy_lease_id)?)
    }

    pub fn mark_recovery_pending(
        &mut self,
        occupancy_lease_id: &str,
        recovery_deadline_at: &Instant,
        now: &Instant,
    ) -> Result<OccupancyLeaseRecord, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .mark_recovery_pending(occupancy_lease_id, recovery_deadline_at, now)?)
    }

    pub fn reconcile_resume(
        &mut self,
        occupancy_lease_id: &str,
        target: OccupancyReconcileTarget,
        idle_expires_at: Option<&Instant>,
        now: &Instant,
    ) -> Result<OccupancyLeaseRecord, ClientOccupancyServiceError> {
        Ok(self.storage.client_occupancy_ledger()?.reconcile_resume(
            occupancy_lease_id,
            target,
            idle_expires_at,
            now,
        )?)
    }

    pub fn force_release(
        &mut self,
        occupancy_lease_id: &str,
        now: &Instant,
    ) -> Result<OccupancyLeaseRecord, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .force_release(occupancy_lease_id, now)?)
    }

    pub fn expire_idle(
        &mut self,
        cutoff: &Instant,
        active_worker_session_count: impl Fn(&str) -> u64,
    ) -> Result<Vec<String>, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .expire_idle(cutoff, active_worker_session_count)?)
    }

    pub fn snapshot(
        &mut self,
        occupancy_lease_id: &str,
    ) -> Result<Option<OccupancyLeaseRecord>, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .snapshot(occupancy_lease_id)?)
    }

    pub fn active_lease_for_node(
        &mut self,
        client_node_id: &str,
    ) -> Result<Option<OccupancyLeaseRecord>, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .active_lease_for_node(client_node_id)?)
    }
}
