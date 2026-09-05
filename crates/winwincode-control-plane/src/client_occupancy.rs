// SPDX-License-Identifier: Apache-2.0

//! `ClientOccupancyLease` application service over the durable Server-side
//! occupancy ledger.
//!
//! The Control Plane is the authoritative owner of client occupancy (ADR-0030,
//! plan 6): it atomically judges the five-condition claim gate, mints the
//! strictly monotonic occupancy fencing tokens, and drives the frozen lease
//! state machine of `docs/contracts/client-control-state-machines.md`
//! contract 4. The Device Client executes occupancy locally: only its ACK of
//! the exact lease and token promotes `reserving -> occupied`, and stale
//! tokens are rejected forever.

use std::fmt;

use winwincode_domain::Instant;
use winwincode_storage::{
    OccupancyClaim, OccupancyLeaseRecord, OccupancyReconcileTarget, OccupancyReleaseReason,
    OccupancyStoreError, OccupancyStoreErrorKind, SqliteStorage,
};

/// Re-exported so service consumers can name the frozen lease states without
/// importing the storage crate directly.
pub use winwincode_storage::OccupancyLeaseState;

/// Stable service failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientOccupancyServiceErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// The client node identity does not exist.
    UnknownClientNode,
    /// No occupancy lease matches the requested identity.
    UnknownOccupancyLease,
    /// The holder has no active `use` access grant on the client node.
    AccessDenied,
    /// The client node presence is not `online`.
    PresenceNotOnline,
    /// The client node is `locked`.
    ClientLocked,
    /// The client node does not accept new occupancy.
    NotAcceptingConnections,
    /// The client node has no free worker-session slot.
    CapacityExhausted,
    /// An active lease already occupies the client node.
    ActiveLeaseConflict,
    /// The occupancy lease id is already used.
    OccupancyLeaseConflict,
    /// The command carried a fencing token other than the lease's token.
    FencingTokenMismatch,
    /// The durable fencing-token counter reached the safe integer ceiling.
    FencingTokenExhausted,
    /// The requested change is not a legal state machine transition.
    IllegalStateTransition,
    /// A compare-and-swap guard lost an impossible race.
    RevisionConflict,
    /// A durable row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free occupancy service error.
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

/// Occupancy application service over one storage connection.
pub struct ClientOccupancyService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> ClientOccupancyService<'storage> {
    /// Builds one service over the sole product-state storage authority.
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Returns the highest occupancy fencing token issued so far.
    ///
    /// # Errors
    ///
    /// Rejects a corrupt counter row or storage failure.
    pub fn current_fencing_token(&mut self) -> Result<u64, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .current_fencing_token()?)
    }

    /// Mints the next global occupancy fencing token; strictly higher than
    /// every earlier token. Only new occupancies mint tokens.
    ///
    /// # Errors
    ///
    /// Rejects a counter at the safe integer ceiling or storage failure.
    pub fn mint_fencing_token(&mut self) -> Result<u64, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .mint_fencing_token()?)
    }

    /// Atomically claims occupancy of one client node (plan 12.2).
    ///
    /// In one immediate transaction the five-condition gate reuses the durable
    /// registry and connect-ledger facts (active `use` grant, `online` and
    /// unlocked presence, accepting connections, no active lease, a free
    /// worker-session slot), mints a new fencing token, and creates the
    /// `reserving` lease. Exactly one of two concurrent claims wins.
    ///
    /// # Errors
    ///
    /// Rejects an unknown client node, a missing or `use`-less grant, a
    /// non-`online` or `locked` node, a node that is not accepting, an
    /// exhausted capacity, an already active lease, a reused lease id, or
    /// storage failure.
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

    /// Records the Device Client ACK (`client.occupancy.ack`) and promotes
    /// `reserving -> occupied` only when the lease id and fencing token both
    /// match (plan 12.2, contract 9.3).
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease, a token mismatch, a non-`reserving` lease,
    /// or storage failure.
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

    /// Terminates a `reserving` lease as `released`, distinguishing ack
    /// timeout, Device Client rejection, and applicant withdrawal through the
    /// release reason (contract 4, contract 10 open point 2).
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease, a token mismatch, a reason that does not
    /// belong to the `reserving` terminal paths, a non-`reserving` lease, or
    /// storage failure.
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

    /// Applies the holder's release request to an `occupied` lease (plan
    /// 12.4): no active worker session releases immediately, any active
    /// worker session moves the lease to `draining`.
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease, a token mismatch, a non-`occupied` lease, or
    /// storage failure.
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

    /// Applies the automatic `draining -> released` judgement once every
    /// worker session reached a terminal state (plan 12.4, contract 4).
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease, a lease that is not `draining`, or storage
    /// failure.
    pub fn drain_complete(
        &mut self,
        occupancy_lease_id: &str,
    ) -> Result<OccupancyLeaseRecord, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .drain_complete(occupancy_lease_id)?)
    }

    /// Projects an `occupied` or `draining` lease to `recovery_pending`
    /// because the client dropped (plan 12.5, contract 4). Requires the
    /// registry presence to already be `offline`; there is no automatic
    /// terminal state and no other user may preempt.
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease or client node, a lease that is neither
    /// `occupied` nor `draining`, a node whose presence is not `offline`, or
    /// storage failure.
    pub fn mark_recovery_pending(
        &mut self,
        occupancy_lease_id: &str,
        recovery_deadline_at: &Instant,
    ) -> Result<OccupancyLeaseRecord, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .mark_recovery_pending(occupancy_lease_id, recovery_deadline_at)?)
    }

    /// Applies an accepted `client.worker.reconcile` outcome (plan 12.5,
    /// contract 4): `recovery_pending -> occupied` or `-> draining`, reusing
    /// the original fencing token unchanged.
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease, a lease that is not `recovery_pending`, an
    /// idle expiry supplied for a draining resume, or storage failure.
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

    /// Releases a `recovery_pending` lease whose recovery window has passed
    /// through the explicit administrator or original-holder safe cleanup
    /// (plan 12.5, contract 4). Occupancy is never handed over automatically.
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease, a lease that is not `recovery_pending`, a
    /// cleanup attempted before the recovery deadline, or storage failure.
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

    /// Expires idle `occupied` leases whose idle policy deadline passed and
    /// that still have no active worker session (plan 12.4, contract 4).
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
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

    /// Returns one durable occupancy lease projection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical lease identity or storage failure.
    pub fn snapshot(
        &mut self,
        occupancy_lease_id: &str,
    ) -> Result<Option<OccupancyLeaseRecord>, ClientOccupancyServiceError> {
        Ok(self
            .storage
            .client_occupancy_ledger()?
            .snapshot(occupancy_lease_id)?)
    }

    /// Returns the one active lease of a client node, if any; `None` means
    /// the occupancy projection is `available`.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical client node identity, a corrupt active-lease
    /// set, or storage failure.
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
