// SPDX-License-Identifier: Apache-2.0

//! Two-phase device scheduler over the durable occupancy, repository ACL,
//! capacity, and launch-grant ledgers (plan FLOW-100.2).
//!
//! `select_and_request_worker` is the service core the `Quick` and
//! `StrongFlow` execution paths call after a user picked a Client and a
//! repository binding. Phase one atomically validates the scheduling
//! preconditions and
//! durably reserves one free worker-session slot of the Client: the caller
//! must hold the node's device-confirmed occupancy (`occupied` or `draining`,
//! per the occupancy service semantics), the binding must be visible through
//! the dual authorization (an active `use` client grant plus an active
//! repository access grant), and the durable capacity ledger — pending
//! reservations plus non-terminal launch grants, reconciled with the
//! device-reported running count — must still have a free slot. The
//! reservation commits behind a compare-and-swap gate inside one immediate
//! transaction, so concurrent `WorkerSession` launches of the same user on
//! the same device can never oversell the capacity.
//!
//! Phase two issues the Worker request itself by reusing the frozen
//! `WorkerLaunchGrantService::issue` gate, which re-validates every phase-one
//! fact atomically and stamps the `issued` grant that takes the slot
//! accounting over; the reservation settles to `granted`. Every failure path
//! releases the reservation durably: a refused launch release-frees the slot
//! immediately, `rollback_launch` rolls a timed-out or cancelled attempt
//! back by revoking the grant, and `expire_stale_reservations` reclaims
//! reservations a crashed scheduler never settled.

use std::fmt;

use sha2::{Digest, Sha256};
use winwincode_domain::Instant;
use winwincode_storage::{
    DeviceSchedulerLedger, DeviceSchedulerReleaseReason, DeviceSchedulerReservationGrant,
    DeviceSchedulerReservationRecord, DeviceSchedulerReservationRelease,
    DeviceSchedulerReservationRequest, DeviceSchedulerReserveOutcome, DeviceSchedulerStoreError,
    DeviceSchedulerStoreErrorKind, LaunchGrantIssuance, SqliteStorage,
};

use crate::ClientOccupancyService;
use crate::ClientRegistryService;
use crate::LaunchGrantState;
use crate::OccupancyLeaseState;
use crate::RepositoryBindingService;
use crate::WorkerLaunchGrantService;
use crate::WorkerLaunchGrantServiceError;
use crate::WorkerLaunchGrantServiceErrorKind;

/// Stable device scheduler failure categories. Each category is secret-free
/// and maps onto exactly one boundary presentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceSchedulerServiceErrorKind {
    /// A scheduling request violated the frozen schema bounds.
    InvalidInput,
    /// The public Client ID does not name a registered, enrolled Client.
    UnknownClientNode,
    /// The Client is not reachable (`offline` or `degraded`).
    PresenceNotOnline,
    /// The Client is locked by a local operator.
    ClientLocked,
    /// The Client has no usable occupancy (none, or not device-confirmed).
    OccupancyRequired,
    /// The occupancy lease belongs to another user.
    NotLeaseHolder,
    /// The occupancy is neither `occupied` nor `draining` (including a stale
    /// fencing stamp the launch gate refused).
    OccupancyNotConfirmed,
    /// The repository binding is unknown, foreign, or invisible to the
    /// holder (uniform dual-authorization rejection).
    BindingNotVisible,
    /// The Client has no free worker-session slot to reserve.
    CapacityExhausted,
    /// The request identity was reused with a different body.
    RequestConflict,
    /// The scheduling request already terminated; retry needs a new request
    /// identity.
    ReservationNotOpen,
    /// No scheduling request matches the requested identity.
    UnknownReservation,
    /// The authoritative launch gate refused the Worker request for a
    /// reason the scheduler does not model separately.
    LaunchRefused,
    /// A compare-and-swap guard lost a race.
    RevisionConflict,
    /// A durable row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free device scheduler service error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceSchedulerServiceError {
    kind: DeviceSchedulerServiceErrorKind,
    message: String,
}

impl DeviceSchedulerServiceError {
    #[must_use]
    pub const fn kind(&self) -> DeviceSchedulerServiceErrorKind {
        self.kind
    }
}

impl fmt::Display for DeviceSchedulerServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DeviceSchedulerServiceError {}

impl From<DeviceSchedulerStoreError> for DeviceSchedulerServiceError {
    fn from(source: DeviceSchedulerStoreError) -> Self {
        Self {
            kind: match source.kind() {
                DeviceSchedulerStoreErrorKind::InvalidInput => {
                    DeviceSchedulerServiceErrorKind::InvalidInput
                }
                DeviceSchedulerStoreErrorKind::UnknownClientNode => {
                    DeviceSchedulerServiceErrorKind::UnknownClientNode
                }
                DeviceSchedulerStoreErrorKind::CapacityExhausted => {
                    DeviceSchedulerServiceErrorKind::CapacityExhausted
                }
                DeviceSchedulerStoreErrorKind::UnknownReservation => {
                    DeviceSchedulerServiceErrorKind::UnknownReservation
                }
                DeviceSchedulerStoreErrorKind::RequestConflict => {
                    DeviceSchedulerServiceErrorKind::RequestConflict
                }
                DeviceSchedulerStoreErrorKind::IllegalStateTransition => {
                    DeviceSchedulerServiceErrorKind::ReservationNotOpen
                }
                DeviceSchedulerStoreErrorKind::RevisionConflict => {
                    DeviceSchedulerServiceErrorKind::RevisionConflict
                }
                DeviceSchedulerStoreErrorKind::CorruptState => {
                    DeviceSchedulerServiceErrorKind::CorruptState
                }
                DeviceSchedulerStoreErrorKind::Storage => DeviceSchedulerServiceErrorKind::Storage,
            },
            message: source.to_string(),
        }
    }
}

/// Validated two-phase scheduling request (plan FLOW-100.2). The boundary
/// mints the worker identities and the short-lived credential digest; the
/// scheduler owns the reservation and launch grant identities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceWorkerSchedulingRequest {
    request_id: String,
    user_id: String,
    public_client_id: String,
    repository_binding_id: String,
    worker_session_id: String,
    worker_id: String,
    worker_instance_id: String,
    credential_digest: String,
    product_session_id: Option<String>,
    stage_run_id: Option<String>,
    expires_at: Instant,
}

impl DeviceWorkerSchedulingRequest {
    /// Builds one validated scheduling request.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical request identity, a non-canonical user,
    /// binding, worker, or session identity, a malformed credential digest,
    /// or a non-canonical expiry.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        request_id: impl Into<String>,
        user_id: impl Into<String>,
        public_client_id: impl Into<String>,
        repository_binding_id: impl Into<String>,
        worker_session_id: impl Into<String>,
        worker_id: impl Into<String>,
        worker_instance_id: impl Into<String>,
        credential_digest: impl Into<String>,
        product_session_id: Option<String>,
        stage_run_id: Option<String>,
        expires_at: Instant,
    ) -> Result<Self, DeviceSchedulerServiceError> {
        let request = Self {
            request_id: request_id.into(),
            user_id: user_id.into(),
            public_client_id: public_client_id.into(),
            repository_binding_id: repository_binding_id.into(),
            worker_session_id: worker_session_id.into(),
            worker_id: worker_id.into(),
            worker_instance_id: worker_instance_id.into(),
            credential_digest: credential_digest.into(),
            product_session_id,
            stage_run_id,
            expires_at,
        };
        validate_prefixed(&request.request_id, "req_", "request id")?;
        validate_prefixed(&request.user_id, "usr_", "user id")?;
        validate_public_client_id(&request.public_client_id)?;
        validate_prefixed(
            &request.repository_binding_id,
            "rbd_",
            "repository binding id",
        )?;
        validate_prefixed(&request.worker_session_id, "ws_", "worker session id")?;
        validate_prefixed(&request.worker_id, "wkr_", "worker id")?;
        validate_prefixed(&request.worker_instance_id, "winst_", "worker instance id")?;
        validate_credential_digest(&request.credential_digest)?;
        if let Some(product) = &request.product_session_id {
            validate_prefixed(product, "ps_", "product session id")?;
        }
        if let Some(stage) = &request.stage_run_id {
            validate_prefixed(stage, "run_", "stage run id")?;
        }
        if request.expires_at.0.len() != 24
            || !request.expires_at.0.ends_with('Z')
            || !request.expires_at.0.starts_with("20")
        {
            return Err(error(
                DeviceSchedulerServiceErrorKind::InvalidInput,
                "grant expiry is not a canonical instant",
            ));
        }
        Ok(request)
    }

    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
}

/// The durable outcome of one two-phase scheduling attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceWorkerSchedulingReceipt {
    pub request_id: String,
    pub device_scheduler_reservation_id: String,
    pub public_client_id: String,
    pub client_node_id: String,
    pub holder_user_id: String,
    pub occupancy_lease_id: String,
    pub occupancy_fencing_token: u64,
    pub repository_binding_id: String,
    pub worker_session_id: String,
    pub worker_id: String,
    pub worker_instance_id: String,
    pub worker_launch_grant_id: String,
    pub product_session_id: Option<String>,
    pub stage_run_id: Option<String>,
    pub expires_at: Instant,
    /// True when the attempt replayed an earlier durable outcome instead of
    /// issuing a second slot.
    pub replayed: bool,
}

/// Device scheduling application service over one storage connection.
pub struct DeviceSchedulerService<'storage> {
    storage: &'storage mut SqliteStorage,
}

/// What phase one produced for one scheduling attempt.
enum ReservedSlot {
    /// A fresh or resumable `reserved` slot awaiting phase two.
    FreshOrResumed(DeviceSchedulerReservationRecord),
    /// The request already completed durably; its receipt is replayed.
    Completed(DeviceWorkerSchedulingReceipt),
}

impl<'storage> DeviceSchedulerService<'storage> {
    /// Builds one service over the sole product-state storage authority.
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Runs the two-phase schedule for one user-picked Client and repository
    /// binding (plan FLOW-100.2).
    ///
    /// Phase one validates the occupancy holder, the device-confirmed
    /// occupancy state, the dual-authorization binding visibility, and the
    /// durable free capacity, then reserves exactly one free slot behind the
    /// compare-and-swap gate. Phase two issues the Worker request through the
    /// frozen launch grant gate and settles the reservation to `granted`.
    /// Every refusal on the way leaves the durable capacity unchanged: a
    /// failed phase two releases the reservation before the error surfaces.
    ///
    /// # Errors
    ///
    /// Returns the stable scheduling failure categories; a replayed request
    /// identity returns the original receipt idempotently instead of issuing
    /// a second slot.
    pub fn select_and_request_worker(
        &mut self,
        request: &DeviceWorkerSchedulingRequest,
        now: &Instant,
    ) -> Result<DeviceWorkerSchedulingReceipt, DeviceSchedulerServiceError> {
        match self.reserve_slot(request, now)? {
            ReservedSlot::Completed(receipt) => Ok(receipt),
            ReservedSlot::FreshOrResumed(reservation) => {
                self.request_worker(request, &reservation, now)
            }
        }
    }

    /// Rolls one scheduling attempt back (plan FLOW-100.2: launch timeout,
    /// cancel): a still-`reserved` slot is released, a `granted` slot is
    /// freed by revoking its `issued` launch grant. Rolling an already
    /// released attempt back is an idempotent no-op.
    ///
    /// # Errors
    ///
    /// Rejects an unknown request identity or storage failure.
    pub fn rollback_launch(
        &mut self,
        request_id: &str,
        now: &Instant,
    ) -> Result<DeviceSchedulerReservationRecord, DeviceSchedulerServiceError> {
        validate_prefixed(request_id, "req_", "request id")?;
        let Some(reservation) = self.reservation_ledger()?.snapshot_by_request(request_id)? else {
            return Err(error(
                DeviceSchedulerServiceErrorKind::UnknownReservation,
                format!("no scheduling request matches {request_id}"),
            ));
        };
        match reservation.state {
            winwincode_storage::DeviceSchedulerReservationState::Released => Ok(reservation),
            winwincode_storage::DeviceSchedulerReservationState::Reserved => {
                let release = DeviceSchedulerReservationRelease::try_new(
                    &reservation.device_scheduler_reservation_id,
                    reservation.revision,
                    DeviceSchedulerReleaseReason::RolledBack,
                )
                .map_err(DeviceSchedulerServiceError::from)?;
                Ok(self.reservation_ledger()?.release(&release, now)?)
            }
            winwincode_storage::DeviceSchedulerReservationState::Granted => {
                let grant_id = reservation.launch_grant_id.clone().ok_or_else(|| {
                    error(
                        DeviceSchedulerServiceErrorKind::CorruptState,
                        "a granted reservation carries no launch grant id",
                    )
                })?;
                let mut grants = WorkerLaunchGrantService::new(self.storage);
                // A rollback of an already rolled back attempt is an
                // idempotent no-op: only a still-`issued` grant revokes.
                let grant = grants
                    .snapshot(&grant_id)
                    .map_err(|_| unavailable())?
                    .ok_or_else(|| {
                        error(
                            DeviceSchedulerServiceErrorKind::CorruptState,
                            "a granted reservation carries an unknown launch grant",
                        )
                    })?;
                if grant.state == LaunchGrantState::Issued {
                    grants
                        .revoke(&grant_id, &reservation.holder_user_id, None, now)
                        .map_err(|revocation_error| map_launch_refusal(&revocation_error))?;
                }
                Ok(reservation)
            }
        }
    }

    /// Reclaims every `reserved` slot whose reservation was created before
    /// `cutoff` (crash safety): the boundary sweeps on a timer so a scheduler
    /// that died between the phases cannot hold the slot forever.
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
    pub fn expire_stale_reservations(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, DeviceSchedulerServiceError> {
        Ok(self.reservation_ledger()?.expire_stale(cutoff)?)
    }

    /// Phase one: validate and durably reserve one free slot. A replayed
    /// request identity resumes a `reserved` slot, returns the receipt of a
    /// `granted` slot idempotently, or refuses a terminated attempt.
    #[allow(clippy::too_many_lines)]
    fn reserve_slot(
        &mut self,
        request: &DeviceWorkerSchedulingRequest,
        now: &Instant,
    ) -> Result<ReservedSlot, DeviceSchedulerServiceError> {
        // Touch the launch grant ledger first so the durable capacity view
        // the reservation CAS judges against exists on a fresh database
        // (the same order production flows open their ledgers in).
        self.storage
            .worker_launch_grant_ledger()
            .map_err(|storage_error| {
                error(
                    DeviceSchedulerServiceErrorKind::Storage,
                    storage_error.to_string(),
                )
            })?;
        // Occupancy: the caller must hold the node's one active lease and it
        // must be device-confirmed (`occupied` or `draining`).
        let (node, lease) = self.validate_holder_and_occupancy(request)?;
        self.validate_binding_visibility(request, &node.client_node_id)?;

        let command = DeviceSchedulerReservationRequest::try_new(
            derive_reservation_id(&request.request_id),
            request.request_id.clone(),
            node.client_node_id.clone(),
            request.user_id.clone(),
            lease.occupancy_lease_id,
            lease.fencing_token,
            request.repository_binding_id.clone(),
            request.worker_session_id.clone(),
        )
        .map_err(DeviceSchedulerServiceError::from)?;
        match self.reservation_ledger()?.reserve(&command, now)? {
            DeviceSchedulerReserveOutcome::Reserved(record) => {
                Ok(ReservedSlot::FreshOrResumed(*record))
            }
            DeviceSchedulerReserveOutcome::Replayed(record) => match record.state {
                winwincode_storage::DeviceSchedulerReservationState::Reserved => {
                    // A previous attempt reserved the slot and died before
                    // the Worker request; resume phase two on this slot.
                    Ok(ReservedSlot::FreshOrResumed(*record))
                }
                winwincode_storage::DeviceSchedulerReservationState::Granted => {
                    // The request completed durably: replay its receipt
                    // instead of issuing a second slot.
                    let grant_id = record.launch_grant_id.clone().ok_or_else(|| {
                        error(
                            DeviceSchedulerServiceErrorKind::CorruptState,
                            "a granted reservation carries no launch grant id",
                        )
                    })?;
                    let grant = WorkerLaunchGrantService::new(self.storage)
                        .snapshot(&grant_id)
                        .map_err(|_| unavailable())?
                        .ok_or_else(|| {
                            error(
                                DeviceSchedulerServiceErrorKind::CorruptState,
                                "a granted reservation carries an unknown launch grant",
                            )
                        })?;
                    Ok(ReservedSlot::Completed(receipt(
                        request,
                        &record,
                        &request.public_client_id,
                        &grant,
                        true,
                    )))
                }
                winwincode_storage::DeviceSchedulerReservationState::Released => Err(error(
                    DeviceSchedulerServiceErrorKind::ReservationNotOpen,
                    "the scheduling request already terminated; retry needs a new request id",
                )),
            },
        }
    }

    /// Phase two: issue the Worker request through the frozen launch grant
    /// gate and settle the reservation to `granted`. Any refusal releases the
    /// reservation before the error surfaces.
    #[allow(clippy::too_many_lines)]
    fn request_worker(
        &mut self,
        request: &DeviceWorkerSchedulingRequest,
        reservation: &DeviceSchedulerReservationRecord,
        now: &Instant,
    ) -> Result<DeviceWorkerSchedulingReceipt, DeviceSchedulerServiceError> {
        let node = {
            let mut registry = ClientRegistryService::new(self.storage);
            registry
                .snapshot(&reservation.client_node_id)
                .map_err(|_| unavailable())?
                .ok_or_else(|| {
                    error(
                        DeviceSchedulerServiceErrorKind::UnknownClientNode,
                        "the reserved client node is gone",
                    )
                })?
        };
        // Crash recovery: when this exact request already produced a live
        // grant (the attempt died between the grant and the settlement),
        // adopt it instead of issuing a second one.
        if let Some(grant) = self.adoptable_grant(request, reservation)? {
            self.settle_granted(reservation, &grant, now)?;
            return Ok(receipt(
                request,
                reservation,
                &node.public_client_id,
                &grant,
                true,
            ));
        }
        let issuance = LaunchGrantIssuance::try_new(
            generate_prefixed_id("wlg_")?,
            reservation.client_node_id.clone(),
            node.current_instance_id.clone().ok_or_else(|| {
                error(
                    DeviceSchedulerServiceErrorKind::CorruptState,
                    "the online client node carries no current instance id",
                )
            })?,
            request.user_id.clone(),
            reservation.occupancy_lease_id.clone(),
            reservation.occupancy_fencing_token,
            reservation.repository_binding_id.clone(),
            request.worker_session_id.clone(),
            request.worker_id.clone(),
            request.worker_instance_id.clone(),
            request.credential_digest.clone(),
            request.product_session_id.clone(),
            request.stage_run_id.clone(),
            request.expires_at.clone(),
        )
        .map_err(|issuance_error| {
            error(
                DeviceSchedulerServiceErrorKind::InvalidInput,
                issuance_error.to_string(),
            )
        })?;
        let issued = match WorkerLaunchGrantService::new(self.storage).issue(&issuance, now) {
            Ok(grant) => grant,
            Err(issue_error) => {
                // The durable slot must not leak: adopt an orphan grant of a
                // crashed earlier attempt, otherwise release the reservation
                // and surface the original refusal.
                return match self.recover_or_release(request, reservation, &issue_error, now)? {
                    Some(recovered) => Ok(recovered),
                    None => Err(map_launch_refusal(&issue_error)),
                };
            }
        };
        self.settle_granted(reservation, &issued, now)?;
        Ok(receipt(
            request,
            reservation,
            &node.public_client_id,
            &issued,
            false,
        ))
    }

    /// Returns the live grant of this exact request identity, if one exists:
    /// the at-most-once adoption handle for an attempt that died between the
    /// grant and the settlement.
    fn adoptable_grant(
        &mut self,
        request: &DeviceWorkerSchedulingRequest,
        reservation: &DeviceSchedulerReservationRecord,
    ) -> Result<Option<winwincode_storage::WorkerLaunchGrantRecord>, DeviceSchedulerServiceError>
    {
        let existing = {
            let mut grants = WorkerLaunchGrantService::new(self.storage);
            grants
                .active_grant_for_session(&request.worker_session_id)
                .map_err(|_| unavailable())?
        };
        Ok(existing.filter(|grant| {
            grant.client_node_id == reservation.client_node_id
                && grant.holder_user_id == request.user_id
                && grant.repository_binding_id == reservation.repository_binding_id
                && grant.worker_session_id == request.worker_session_id
                && grant.worker_id == request.worker_id
                && grant.worker_instance_id == request.worker_instance_id
        }))
    }

    /// Settles the reservation to `granted` after the grant committed.
    fn settle_granted(
        &mut self,
        reservation: &DeviceSchedulerReservationRecord,
        grant: &winwincode_storage::WorkerLaunchGrantRecord,
        now: &Instant,
    ) -> Result<(), DeviceSchedulerServiceError> {
        let settlement = DeviceSchedulerReservationGrant::try_new(
            &reservation.device_scheduler_reservation_id,
            &grant.worker_launch_grant_id,
            reservation.revision,
        )
        .map_err(DeviceSchedulerServiceError::from)?;
        if let Err(settle_error) = self.reservation_ledger()?.settle_granted(&settlement, now) {
            // The grant is live but the reservation could not follow; revoke
            // the grant so the slot accounting stays consistent, then surface
            // the settlement failure.
            let mut grants = WorkerLaunchGrantService::new(self.storage);
            let _ = grants.revoke(
                &grant.worker_launch_grant_id,
                &reservation.holder_user_id,
                None,
                now,
            );
            return Err(settle_error.into());
        }
        Ok(())
    }

    /// Crash-recovery gate for a refused phase two: when the worker session
    /// already carries a live grant that matches this exact request (a
    /// previous attempt died between the grant and the settlement), the
    /// reservation settles onto it; otherwise the reservation is released so
    /// the slot is free again, and `None` tells the caller to surface the
    /// original issue error.
    fn recover_or_release(
        &mut self,
        request: &DeviceWorkerSchedulingRequest,
        reservation: &DeviceSchedulerReservationRecord,
        issue_error: &WorkerLaunchGrantServiceError,
        now: &Instant,
    ) -> Result<Option<DeviceWorkerSchedulingReceipt>, DeviceSchedulerServiceError> {
        if issue_error.kind() != WorkerLaunchGrantServiceErrorKind::LaunchGrantConflict {
            self.release_reservation(reservation, DeviceSchedulerReleaseReason::LaunchFailed, now)?;
            return Ok(None);
        }
        let orphan = {
            let mut grants = WorkerLaunchGrantService::new(self.storage);
            grants
                .active_grant_for_session(&request.worker_session_id)
                .map_err(|_| unavailable())?
        };
        let matches_request = orphan.as_ref().is_some_and(|grant| {
            grant.client_node_id == reservation.client_node_id
                && grant.holder_user_id == request.user_id
                && grant.repository_binding_id == request.repository_binding_id
                && grant.worker_session_id == request.worker_session_id
                && grant.worker_id == request.worker_id
                && grant.worker_instance_id == request.worker_instance_id
        });
        if !matches_request {
            self.release_reservation(reservation, DeviceSchedulerReleaseReason::LaunchFailed, now)?;
            return Ok(None);
        }
        let orphan = orphan.expect("the live grant was just matched");
        self.settle_granted(reservation, &orphan, now)?;
        Ok(Some(receipt(
            request,
            reservation,
            &request.public_client_id,
            &orphan,
            true,
        )))
    }

    /// Releases a `reserved` slot; a release failure must not hide the
    /// original error, so it surfaces as the storage category.
    fn release_reservation(
        &mut self,
        reservation: &DeviceSchedulerReservationRecord,
        reason: DeviceSchedulerReleaseReason,
        now: &Instant,
    ) -> Result<(), DeviceSchedulerServiceError> {
        let release = DeviceSchedulerReservationRelease::try_new(
            &reservation.device_scheduler_reservation_id,
            reservation.revision,
            reason,
        )
        .map_err(DeviceSchedulerServiceError::from)?;
        if let Err(release_error) = self.reservation_ledger()?.release(&release, now) {
            return Err(DeviceSchedulerServiceError::from(release_error));
        }
        Ok(())
    }

    fn validate_holder_and_occupancy(
        &mut self,
        request: &DeviceWorkerSchedulingRequest,
    ) -> Result<
        (
            winwincode_storage::ClientNodeRecord,
            winwincode_storage::OccupancyLeaseRecord,
        ),
        DeviceSchedulerServiceError,
    > {
        let node = {
            let mut registry = ClientRegistryService::new(self.storage);
            let record = registry
                .snapshot_by_public_client_id(&request.public_client_id)
                .map_err(|_| unavailable())?;
            match record {
                None
                | Some(winwincode_storage::ClientNodeRecord {
                    presence_state:
                        winwincode_storage::ClientPresenceState::PendingEnrollment
                        | winwincode_storage::ClientPresenceState::Revoked,
                    ..
                }) => {
                    return Err(error(
                        DeviceSchedulerServiceErrorKind::UnknownClientNode,
                        "no client matches the requested id",
                    ));
                }
                Some(node)
                    if matches!(
                        node.presence_state,
                        winwincode_storage::ClientPresenceState::Offline
                            | winwincode_storage::ClientPresenceState::Degraded
                    ) =>
                {
                    return Err(error(
                        DeviceSchedulerServiceErrorKind::PresenceNotOnline,
                        "the client is not online",
                    ));
                }
                Some(node)
                    if node.presence_state == winwincode_storage::ClientPresenceState::Locked =>
                {
                    return Err(error(
                        DeviceSchedulerServiceErrorKind::ClientLocked,
                        "the client is locked",
                    ));
                }
                Some(node) => node,
            }
        };
        let lease = {
            let mut occupancy = ClientOccupancyService::new(self.storage);
            occupancy
                .active_lease_for_node(&node.client_node_id)
                .map_err(|_| unavailable())?
        };
        let Some(lease) = lease else {
            return Err(error(
                DeviceSchedulerServiceErrorKind::OccupancyRequired,
                "the client is not occupied; claim occupancy before scheduling",
            ));
        };
        if lease.holder_user_id != request.user_id {
            return Err(error(
                DeviceSchedulerServiceErrorKind::NotLeaseHolder,
                "only the occupancy holder may request a worker session",
            ));
        }
        if !matches!(
            lease.state,
            OccupancyLeaseState::Occupied | OccupancyLeaseState::Draining
        ) {
            return Err(error(
                DeviceSchedulerServiceErrorKind::OccupancyNotConfirmed,
                "the occupancy is not confirmed by the device",
            ));
        }
        Ok((node, lease))
    }

    fn validate_binding_visibility(
        &mut self,
        request: &DeviceWorkerSchedulingRequest,
        client_node_id: &str,
    ) -> Result<(), DeviceSchedulerServiceError> {
        let mut repository = RepositoryBindingService::new(self.storage);
        let visible = repository
            .visible_bindings(&request.user_id, client_node_id)
            .map_err(|_| unavailable())?;
        if visible
            .iter()
            .any(|binding| binding.repository_binding_id == request.repository_binding_id)
        {
            Ok(())
        } else {
            Err(error(
                DeviceSchedulerServiceErrorKind::BindingNotVisible,
                "the repository binding is not visible to the holder",
            ))
        }
    }

    fn reservation_ledger(
        &mut self,
    ) -> Result<DeviceSchedulerLedger<'_>, DeviceSchedulerServiceError> {
        Ok(self.storage.device_scheduler_reservation_ledger()?)
    }
}

/// Assembles the durable receipt of one scheduled Worker request.
fn receipt(
    request: &DeviceWorkerSchedulingRequest,
    reservation: &DeviceSchedulerReservationRecord,
    public_client_id: &str,
    grant: &winwincode_storage::WorkerLaunchGrantRecord,
    replayed: bool,
) -> DeviceWorkerSchedulingReceipt {
    DeviceWorkerSchedulingReceipt {
        request_id: request.request_id.clone(),
        device_scheduler_reservation_id: reservation.device_scheduler_reservation_id.clone(),
        public_client_id: public_client_id.to_owned(),
        client_node_id: grant.client_node_id.clone(),
        holder_user_id: grant.holder_user_id.clone(),
        occupancy_lease_id: grant.occupancy_lease_id.clone(),
        occupancy_fencing_token: grant.occupancy_fencing_token,
        repository_binding_id: grant.repository_binding_id.clone(),
        worker_session_id: grant.worker_session_id.clone(),
        worker_id: grant.worker_id.clone(),
        worker_instance_id: grant.worker_instance_id.clone(),
        worker_launch_grant_id: grant.worker_launch_grant_id.clone(),
        product_session_id: grant.product_session_id.clone(),
        stage_run_id: grant.stage_run_id.clone(),
        expires_at: grant.expires_at.clone(),
        replayed,
    }
}

/// Maps one launch-gate refusal onto the stable scheduling taxonomy.
fn map_launch_refusal(source: &WorkerLaunchGrantServiceError) -> DeviceSchedulerServiceError {
    let kind = match source.kind() {
        WorkerLaunchGrantServiceErrorKind::InvalidInput => {
            DeviceSchedulerServiceErrorKind::InvalidInput
        }
        WorkerLaunchGrantServiceErrorKind::UnknownClientNode => {
            DeviceSchedulerServiceErrorKind::UnknownClientNode
        }
        WorkerLaunchGrantServiceErrorKind::PresenceNotOnline => {
            DeviceSchedulerServiceErrorKind::PresenceNotOnline
        }
        WorkerLaunchGrantServiceErrorKind::ClientLocked => {
            DeviceSchedulerServiceErrorKind::ClientLocked
        }
        WorkerLaunchGrantServiceErrorKind::UnknownOccupancyLease => {
            DeviceSchedulerServiceErrorKind::OccupancyRequired
        }
        WorkerLaunchGrantServiceErrorKind::NotLeaseHolder => {
            DeviceSchedulerServiceErrorKind::NotLeaseHolder
        }
        WorkerLaunchGrantServiceErrorKind::OccupancyNotConfirmed
        | WorkerLaunchGrantServiceErrorKind::FencingTokenMismatch => {
            DeviceSchedulerServiceErrorKind::OccupancyNotConfirmed
        }
        WorkerLaunchGrantServiceErrorKind::UnknownRepositoryBinding
        | WorkerLaunchGrantServiceErrorKind::BindingForeignClient
        | WorkerLaunchGrantServiceErrorKind::BindingNotVisible => {
            DeviceSchedulerServiceErrorKind::BindingNotVisible
        }
        WorkerLaunchGrantServiceErrorKind::CapacityExhausted => {
            DeviceSchedulerServiceErrorKind::CapacityExhausted
        }
        WorkerLaunchGrantServiceErrorKind::LaunchGrantConflict
        | WorkerLaunchGrantServiceErrorKind::GrantExpired
        | WorkerLaunchGrantServiceErrorKind::UnknownLaunchGrant
        | WorkerLaunchGrantServiceErrorKind::FieldMismatch
        | WorkerLaunchGrantServiceErrorKind::IllegalStateTransition => {
            DeviceSchedulerServiceErrorKind::LaunchRefused
        }
        WorkerLaunchGrantServiceErrorKind::RevisionConflict => {
            DeviceSchedulerServiceErrorKind::RevisionConflict
        }
        WorkerLaunchGrantServiceErrorKind::CorruptState => {
            DeviceSchedulerServiceErrorKind::CorruptState
        }
        WorkerLaunchGrantServiceErrorKind::Storage => DeviceSchedulerServiceErrorKind::Storage,
    };
    error(kind, source.to_string())
}

fn unavailable() -> DeviceSchedulerServiceError {
    error(
        DeviceSchedulerServiceErrorKind::Storage,
        "device scheduler service is unavailable",
    )
}

fn error(
    kind: DeviceSchedulerServiceErrorKind,
    message: impl Into<String>,
) -> DeviceSchedulerServiceError {
    DeviceSchedulerServiceError {
        kind,
        message: message.into(),
    }
}

fn validate_prefixed(
    value: &str,
    prefix: &str,
    label: &str,
) -> Result<(), DeviceSchedulerServiceError> {
    let valid = value
        .strip_prefix(prefix)
        .is_some_and(|suffix| suffix.len() == 26 && suffix.bytes().all(is_crockford_base32));
    if valid {
        Ok(())
    } else {
        Err(error(
            DeviceSchedulerServiceErrorKind::InvalidInput,
            format!("{label} is not canonical"),
        ))
    }
}

fn validate_public_client_id(value: &str) -> Result<(), DeviceSchedulerServiceError> {
    if (9..=12).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(())
    } else {
        Err(error(
            DeviceSchedulerServiceErrorKind::InvalidInput,
            "the public client id is not 9-12 ascii digits",
        ))
    }
}

fn validate_credential_digest(value: &str) -> Result<(), DeviceSchedulerServiceError> {
    if value.len() == 71
        && value.starts_with("sha256:")
        && value["sha256:".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(error(
            DeviceSchedulerServiceErrorKind::InvalidInput,
            "the credential digest is not a lowercase sha256 digest",
        ))
    }
}

const fn is_crockford_base32(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
    )
}

/// Crockford Base32 alphabet shared with the canonical identity encodings.
const IDENTITY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Derives the reservation identity deterministically from the request
/// identity: a replayed request computes the byte-identical reservation
/// command, so the durable replay gate recognizes it.
#[must_use]
fn derive_reservation_id(request_id: &str) -> String {
    let digest = Sha256::digest(request_id.as_bytes());
    let mut identity = String::with_capacity(4 + 26);
    identity.push_str("dsr_");
    for byte in digest.iter().take(13) {
        let byte = *byte;
        identity.push(IDENTITY_ALPHABET[usize::from(byte & 0x0f)] as char);
        identity.push(IDENTITY_ALPHABET[usize::from(byte >> 4)] as char);
    }
    identity
}

/// Generates one canonical `prefix` + 26 character Crockford identifier.
fn generate_prefixed_id(prefix: &str) -> Result<String, DeviceSchedulerServiceError> {
    let mut random = [0_u8; 13];
    getrandom::fill(&mut random).map_err(|_| unavailable())?;
    let mut identity = String::with_capacity(prefix.len() + 26);
    identity.push_str(prefix);
    for byte in random {
        identity.push(IDENTITY_ALPHABET[usize::from(byte >> 4)] as char);
        identity.push(IDENTITY_ALPHABET[usize::from(byte & 0x0f)] as char);
    }
    Ok(identity)
}
