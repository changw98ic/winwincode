// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use sha2::{Digest, Sha256};
use winwincode_domain::{Instant, WorkRunId};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceSchedulerServiceErrorKind {
    InvalidInput,
    UnknownClientNode,
    PresenceNotOnline,
    ClientLocked,
    OccupancyRequired,
    NotLeaseHolder,
    OccupancyNotConfirmed,
    BindingNotVisible,
    CapacityExhausted,
    RequestConflict,
    ReservationNotOpen,
    UnknownReservation,
    LaunchRefused,
    RevisionConflict,
    CorruptState,
    Storage,
}

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceWorkerSchedulingRequest {
    request_id: String,
    user_id: String,
    public_client_id: String,
    repository_binding_id: String,
    worker_session_id: String,
    worker_id: String,
    worker_instance_id: String,
    product_session_id: Option<String>,
    work_run_id: Option<String>,
    expires_at: Instant,
}

impl DeviceWorkerSchedulingRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        request_id: impl Into<String>,
        user_id: impl Into<String>,
        public_client_id: impl Into<String>,
        repository_binding_id: impl Into<String>,
        worker_session_id: impl Into<String>,
        worker_id: impl Into<String>,
        worker_instance_id: impl Into<String>,
        product_session_id: Option<String>,
        work_run_id: Option<String>,
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
            product_session_id,
            work_run_id,
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
        validate_prefixed(&request.worker_session_id, "wsn_", "worker session id")?;
        validate_prefixed(&request.worker_id, "wrk_", "worker id")?;
        validate_prefixed(&request.worker_instance_id, "wki_", "worker instance id")?;
        if let Some(product) = &request.product_session_id {
            validate_prefixed(product, "ps_", "product session id")?;
        }
        if let Some(work_run) = &request.work_run_id {
            validate_prefixed(work_run, "wrn_", "WorkRun id")?;
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
    pub work_run_id: Option<String>,
    pub expires_at: Instant,
    pub replayed: bool,
}

pub struct DeviceSchedulerService<'storage> {
    storage: &'storage mut SqliteStorage,
}

enum ReservedSlot {
    FreshOrResumed(DeviceSchedulerReservationRecord),
    Completed(DeviceWorkerSchedulingReceipt),
}

impl<'storage> DeviceSchedulerService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

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

    pub fn expire_stale_reservations(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, DeviceSchedulerServiceError> {
        Ok(self.reservation_ledger()?.expire_stale(cutoff)?)
    }

    #[allow(clippy::too_many_lines)]
    fn reserve_slot(
        &mut self,
        request: &DeviceWorkerSchedulingRequest,
        now: &Instant,
    ) -> Result<ReservedSlot, DeviceSchedulerServiceError> {
        self.storage
            .worker_launch_grant_ledger()
            .map_err(|storage_error| {
                error(
                    DeviceSchedulerServiceErrorKind::Storage,
                    storage_error.to_string(),
                )
            })?;
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
                    Ok(ReservedSlot::FreshOrResumed(*record))
                }
                winwincode_storage::DeviceSchedulerReservationState::Granted => {
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
            request.product_session_id.clone(),
            request.work_run_id.clone().map(WorkRunId),
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
        work_run_id: grant.work_run_id.as_ref().map(|id| id.0.clone()),
        expires_at: grant.expires_at.clone(),
        replayed,
    }
}

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

const fn is_crockford_base32(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
    )
}

const IDENTITY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

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
