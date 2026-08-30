// SPDX-License-Identifier: Apache-2.0

//! Enterprise quota choreography around the existing Worker claim authority.
//!
//! This module does not own Worker placement, Registry claims, or operational
//! reservations. It derives enterprise attribution from durable job and
//! admission receipts joined to an exact Worker session plus authenticated
//! principal, reserves before the existing claim, and settles only from an
//! immutable Worker Usage entry already projected to the enterprise ledger.

use std::fmt;

use sha2::{Digest, Sha256};
use winwincode_domain::{ExecutionJobId, Instant, RequestId};
use winwincode_storage::{
    AuthenticatedWorkerPlacement, EnterpriseQuotaAmounts, EnterpriseQuotaError,
    EnterpriseQuotaRelease, EnterpriseQuotaReleaseReason, EnterpriseQuotaReservationReceipt,
    EnterpriseQuotaReservationRequest, EnterpriseQuotaSourceSeal, EnterpriseUsageAttribution,
    EnterpriseUsageEntry, EnterpriseUsageSource, ExecutionJobRecord, ExecutionJobState,
    ExecutionLeaseClaim, ExecutionReservationRecord, ExecutionReservationState,
    WorkerRegistryScope,
};

use crate::{EnterpriseQuotaAdmission, EnterpriseQuotaAdmissionPort, EnterpriseQuotaPermit};

/// Stable Worker quota failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerEnterpriseQuotaErrorKind {
    AuthorityMissing,
    AuthorityMismatch,
    AuthorityUnavailable,
    Quota,
    OperationalClaim,
    Rollback,
    UsageMissing,
    UsageMismatch,
    UsageUnavailable,
}

/// Secret-free Worker quota orchestration error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerEnterpriseQuotaError {
    kind: WorkerEnterpriseQuotaErrorKind,
}

impl WorkerEnterpriseQuotaError {
    pub(crate) const fn new(kind: WorkerEnterpriseQuotaErrorKind) -> Self {
        Self { kind }
    }

    /// Returns the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> WorkerEnterpriseQuotaErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerEnterpriseQuotaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Worker enterprise quota operation failed")
    }
}

impl std::error::Error for WorkerEnterpriseQuotaError {}

/// Immutable authority for one Worker claim derived only from durable records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerEnterpriseQuotaAuthority {
    job: ExecutionJobRecord,
    admission: ExecutionReservationRecord,
    placement: AuthenticatedWorkerPlacement,
    claim: ExecutionLeaseClaim,
}

impl WorkerEnterpriseQuotaAuthority {
    /// Joins the durable Job and admission receipts to the transport-
    /// authenticated pool placement and exact Registry claim.
    ///
    /// # Errors
    ///
    /// Rejects changed job/scope/Worker/pool facts. The pool comes only from
    /// the authenticated placement receipt and must exactly match durable admission.
    pub fn from_durable_records(
        job: ExecutionJobRecord,
        admission: ExecutionReservationRecord,
        placement: AuthenticatedWorkerPlacement,
        claim: ExecutionLeaseClaim,
    ) -> Result<Self, WorkerEnterpriseQuotaError> {
        if job.job_id != admission.job_id
            || job.scope != admission.scope
            || job.job_id != claim.job_id
            || job.payload_digest != claim.payload_digest
            || job.attempt != claim.attempt
            || placement.worker_id != claim.worker_id
            || placement.worker_instance_id != claim.worker_instance_id
            || admission.worker_pool_id != placement.worker_pool_id
            || !placement_scope_contains(&placement.management_scope, &job)
            || matches!(
                job.state,
                ExecutionJobState::Completed | ExecutionJobState::Failed
            )
            || admission.state == ExecutionReservationState::Settled
        {
            return Err(WorkerEnterpriseQuotaError::new(
                WorkerEnterpriseQuotaErrorKind::AuthorityMismatch,
            ));
        }
        Ok(Self {
            job,
            admission,
            placement,
            claim,
        })
    }

    /// Returns the exact durable job identity.
    #[must_use]
    pub const fn job_id(&self) -> &ExecutionJobId {
        &self.job.job_id
    }

    /// Returns the exact Registry claim joined to the durable authority.
    #[must_use]
    pub const fn claim(&self) -> &ExecutionLeaseClaim {
        &self.claim
    }

    /// Returns the authenticated pool placement frozen before claiming.
    #[must_use]
    pub const fn placement(&self) -> &AuthenticatedWorkerPlacement {
        &self.placement
    }

    /// Returns the exact scheduler Job receipt.
    #[must_use]
    pub const fn job(&self) -> &ExecutionJobRecord {
        &self.job
    }

    /// Returns the exact operational admission reservation.
    #[must_use]
    pub const fn admission(&self) -> &ExecutionReservationRecord {
        &self.admission
    }

    fn attribution(&self) -> EnterpriseUsageAttribution {
        EnterpriseUsageAttribution {
            organization_id: self.admission.scope.organization_id.clone(),
            workspace_id: self.admission.scope.workspace_id.clone(),
            project_id: self.admission.scope.project_id.clone(),
            repository_id: self.admission.scope.repository_id.clone(),
            delivery_id: self.admission.scope.delivery_id.clone(),
            product_session_id: Some(self.admission.scope.product_session_id.clone()),
            user_id: self.admission.user_id.clone(),
        }
    }

    fn amounts(&self) -> EnterpriseQuotaAmounts {
        EnterpriseQuotaAmounts {
            tokens: self.admission.reserved_tokens,
            worker_cost_microunits: self.admission.reserved_cost_microunits,
            worker_runtime_millis: self.admission.runtime_limit_millis,
            operations: 1,
            ..EnterpriseQuotaAmounts::default()
        }
    }
}

/// Resolves the exact durable Worker authority for one claim attempt.
pub trait WorkerEnterpriseQuotaAuthorityPort {
    /// Loads authority by job and session identity.
    ///
    /// # Errors
    ///
    /// Returns unavailable for an unreadable authority store. `None` means the
    /// job/session join is missing and must fail closed.
    fn load(
        &mut self,
        job_id: &ExecutionJobId,
        claim: &ExecutionLeaseClaim,
    ) -> Result<Option<WorkerEnterpriseQuotaAuthority>, WorkerEnterpriseQuotaError>;
}

/// Existing Registry or scheduler claim invoked after enterprise reservation.
pub trait WorkerOperationalClaimPort {
    type Receipt;

    /// Claims the existing operational capacity for this exact authority.
    ///
    /// # Errors
    ///
    /// Returns the existing claim authority's bounded failure.
    fn claim(
        &mut self,
        authority: &WorkerEnterpriseQuotaAuthority,
    ) -> Result<Self::Receipt, WorkerEnterpriseQuotaError>;
}

/// Reads immutable Worker Usage already projected into the enterprise ledger.
pub trait WorkerEnterpriseUsageSourcePort {
    /// Loads one immutable source by the exact trusted claim authority.
    ///
    /// # Errors
    ///
    /// Returns unavailable when the projection cannot be read. `None` means the
    /// source has not been projected yet and settlement must remain pending.
    fn load(
        &mut self,
        authority: &WorkerEnterpriseQuotaAuthority,
    ) -> Result<Option<EnterpriseUsageEntry>, WorkerEnterpriseQuotaError>;
}

/// Active enterprise reservation bound to a trusted Worker authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerEnterpriseQuotaReservation {
    authority: WorkerEnterpriseQuotaAuthority,
    request: EnterpriseQuotaReservationRequest,
    permit: EnterpriseQuotaPermit,
}

impl WorkerEnterpriseQuotaReservation {
    /// Returns the durable enterprise reservation receipt.
    #[must_use]
    pub const fn receipt(&self) -> &EnterpriseQuotaReservationReceipt {
        self.permit.receipt()
    }
}

/// Result of enterprise reservation followed by the existing Worker claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerEnterpriseQuotaClaim<Receipt> {
    Claimed {
        reservation: Box<WorkerEnterpriseQuotaReservation>,
        operational: Receipt,
    },
    TerminalReplay(Box<EnterpriseQuotaReservationReceipt>),
    Denied,
}

/// Coordinates the unique enterprise quota port with the existing Worker claim.
pub struct WorkerEnterpriseQuotaSaga<'quota> {
    quota: &'quota mut dyn EnterpriseQuotaAdmissionPort,
}

impl<'quota> WorkerEnterpriseQuotaSaga<'quota> {
    /// Creates the Worker quota orchestrator.
    #[must_use]
    pub fn new(quota: &'quota mut dyn EnterpriseQuotaAdmissionPort) -> Self {
        Self { quota }
    }

    /// Reserves enterprise quota before the existing Registry/scheduler claim.
    ///
    /// # Errors
    ///
    /// Fails closed for a missing or changed durable authority, quota failure,
    /// operational claim failure, or failed compensating release.
    pub fn reserve_then_claim<Operational>(
        &mut self,
        authorities: &mut dyn WorkerEnterpriseQuotaAuthorityPort,
        job_id: &ExecutionJobId,
        claim: &ExecutionLeaseClaim,
        requested_at: Instant,
        operational: &mut Operational,
    ) -> Result<WorkerEnterpriseQuotaClaim<Operational::Receipt>, WorkerEnterpriseQuotaError>
    where
        Operational: WorkerOperationalClaimPort,
    {
        let authority = authorities.load(job_id, claim)?.ok_or_else(|| {
            WorkerEnterpriseQuotaError::new(WorkerEnterpriseQuotaErrorKind::AuthorityMissing)
        })?;
        if authority.job_id() != job_id || authority.claim() != claim {
            return Err(WorkerEnterpriseQuotaError::new(
                WorkerEnterpriseQuotaErrorKind::AuthorityMismatch,
            ));
        }
        let request = reservation_request(&authority, requested_at.clone());
        match self
            .quota
            .reserve(&request)
            .map_err(|error| map_quota(&error))?
        {
            EnterpriseQuotaAdmission::Admitted(permit) => {
                let Ok(receipt) = operational.claim(&authority) else {
                    self.quota
                        .release(&EnterpriseQuotaRelease {
                            reservation_id: request.reservation_id.clone(),
                            request_id: quota_request_id("release", &request.reservation_id.0),
                            expected_revision: permit.receipt().record.revision,
                            reason: EnterpriseQuotaReleaseReason::OperationalAdmissionDenied,
                            released_at: requested_at,
                        })
                        .map_err(|_| {
                            WorkerEnterpriseQuotaError::new(
                                WorkerEnterpriseQuotaErrorKind::Rollback,
                            )
                        })?;
                    return Err(WorkerEnterpriseQuotaError::new(
                        WorkerEnterpriseQuotaErrorKind::OperationalClaim,
                    ));
                };
                Ok(WorkerEnterpriseQuotaClaim::Claimed {
                    reservation: Box::new(WorkerEnterpriseQuotaReservation {
                        authority,
                        request,
                        permit: *permit,
                    }),
                    operational: receipt,
                })
            }
            EnterpriseQuotaAdmission::TerminalReplay(receipt) => {
                Ok(WorkerEnterpriseQuotaClaim::TerminalReplay(receipt))
            }
            EnterpriseQuotaAdmission::Denied(_) => Ok(WorkerEnterpriseQuotaClaim::Denied),
        }
    }

    /// Releases an exact active reservation after failure or cancellation.
    ///
    /// # Errors
    ///
    /// Returns a bounded quota error for a stale, changed, or unavailable terminal write.
    pub fn release(
        &mut self,
        reservation: &WorkerEnterpriseQuotaReservation,
        reason: EnterpriseQuotaReleaseReason,
        released_at: Instant,
    ) -> Result<EnterpriseQuotaReservationReceipt, WorkerEnterpriseQuotaError> {
        self.quota
            .release(&EnterpriseQuotaRelease {
                reservation_id: reservation.request.reservation_id.clone(),
                request_id: quota_request_id("release", &reservation.request.reservation_id.0),
                expected_revision: reservation.permit.receipt().record.revision,
                reason,
                released_at,
            })
            .map_err(|error| map_quota(&error))
    }

    /// Settles only when immutable Worker Usage has reached the enterprise ledger.
    ///
    /// # Errors
    ///
    /// Fails closed while the source is absent, unavailable, or differs from the
    /// durable Worker authority/reservation source seal.
    pub fn settle_from_usage_source(
        &mut self,
        reservation: &WorkerEnterpriseQuotaReservation,
        sources: &mut dyn WorkerEnterpriseUsageSourcePort,
    ) -> Result<EnterpriseQuotaReservationReceipt, WorkerEnterpriseQuotaError> {
        let entry = sources.load(&reservation.authority)?.ok_or_else(|| {
            WorkerEnterpriseQuotaError::new(WorkerEnterpriseQuotaErrorKind::UsageMissing)
        })?;
        if !usage_matches(&reservation.authority, &entry) {
            return Err(WorkerEnterpriseQuotaError::new(
                WorkerEnterpriseQuotaErrorKind::UsageMismatch,
            ));
        }
        self.quota
            .settle_usage_source(&entry.fact.source)
            .map_err(|error| map_quota(&error))?
            .ok_or_else(|| {
                WorkerEnterpriseQuotaError::new(WorkerEnterpriseQuotaErrorKind::UsageMismatch)
            })
    }
}

fn placement_scope_contains(placement: &WorkerRegistryScope, job: &ExecutionJobRecord) -> bool {
    match placement {
        WorkerRegistryScope::Organization { organization_id } => {
            organization_id == &job.scope.organization_id
        }
        WorkerRegistryScope::Workspace {
            organization_id,
            workspace_id,
        } => {
            organization_id == &job.scope.organization_id && workspace_id == &job.scope.workspace_id
        }
        WorkerRegistryScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => {
            organization_id == &job.scope.organization_id
                && workspace_id == &job.scope.workspace_id
                && project_id == &job.scope.project_id
        }
        WorkerRegistryScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => {
            organization_id == &job.scope.organization_id
                && workspace_id == &job.scope.workspace_id
                && project_id == &job.scope.project_id
                && repository_id == &job.scope.repository_id
        }
    }
}

fn reservation_request(
    authority: &WorkerEnterpriseQuotaAuthority,
    requested_at: Instant,
) -> EnterpriseQuotaReservationRequest {
    EnterpriseQuotaReservationRequest {
        reservation_id: quota_request_id("reserve", &authority.job_id().0),
        attribution: authority.attribution(),
        source_seal: EnterpriseQuotaSourceSeal::Worker {
            job_id: authority.job.job_id.clone(),
            worker_pool_id: authority.admission.worker_pool_id.0.clone(),
        },
        reserved: authority.amounts(),
        requested_at,
    }
}

fn usage_matches(authority: &WorkerEnterpriseQuotaAuthority, entry: &EnterpriseUsageEntry) -> bool {
    let attribution = authority.attribution();
    let source_matches = matches!(
        &entry.fact.source,
        EnterpriseUsageSource::Worker { job_id, worker_pool_id, .. }
            if job_id == authority.job_id() && worker_pool_id == &authority.admission.worker_pool_id.0
    );
    entry.fact.attribution == attribution && source_matches
}

fn quota_request_id(action: &str, identity: &str) -> RequestId {
    let digest = Sha256::digest(
        [
            b"winwincode.worker-enterprise-quota.v1\0".as_slice(),
            action.as_bytes(),
            b"\0".as_slice(),
            identity.as_bytes(),
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

fn map_quota(_error: &EnterpriseQuotaError) -> WorkerEnterpriseQuotaError {
    WorkerEnterpriseQuotaError::new(WorkerEnterpriseQuotaErrorKind::Quota)
}
