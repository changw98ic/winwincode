// SPDX-License-Identifier: Apache-2.0

//! Enterprise quota choreography for one immutable Artifact finalization.
//!
//! The caller may construct the [`ArtifactOpen`] only after joining the Worker
//! message to its durable `ExecutionJob`, `SessionBinding`, authenticated User,
//! and repository scope. This module deliberately accepts that frozen object,
//! not caller-supplied organization or User fields. A completed Artifact emits
//! one immutable storage source; that source is recorded in the enterprise
//! Usage ledger before the matching reservation is settled.

use std::fmt;
use std::path::Path;

use winwincode_domain::{ArtifactId, ExecutionJobId, Instant};
use winwincode_storage::{
    ArtifactError, ArtifactOpen, ArtifactStorageOperationKind, ArtifactStorageSourceEntry,
    ArtifactStore, EnterpriseQuotaAmounts, EnterpriseQuotaDenial, EnterpriseQuotaError,
    EnterpriseQuotaRelease, EnterpriseQuotaReleaseReason, EnterpriseQuotaReservationReceipt,
    EnterpriseQuotaReservationRequest, EnterpriseQuotaReservationState, EnterpriseQuotaSettlement,
    EnterpriseQuotaSourceSeal, EnterpriseUsageAttribution, EnterpriseUsageError,
    EnterpriseUsageMeasure, EnterpriseUsageReceipt, EnterpriseUsageSource, ProductStateStorage,
    SettledEnterpriseUsage, SqliteStorage, StorageError,
};

use crate::{EnterpriseQuotaAdmission, EnterpriseQuotaAdmissionPort};

/// Records a final Artifact source in the one immutable enterprise Usage ledger.
pub trait ArtifactEnterpriseUsagePort: Send {
    /// Records the exact immutable source and returns its durable ledger receipt.
    ///
    /// # Errors
    ///
    /// Returns an error when the source conflicts with its original fact or the
    /// durable Usage ledger is unavailable.
    fn record_storage_source(
        &mut self,
        source: &ArtifactStorageSourceEntry,
    ) -> Result<EnterpriseUsageReceipt, EnterpriseUsageError>;
}

/// Production Artifact-to-enterprise-Usage recorder over the canonical database.
pub struct DurableArtifactEnterpriseUsage {
    storage: SqliteStorage,
}

impl DurableArtifactEnterpriseUsage {
    /// Creates a durable storage Usage recorder.
    #[must_use]
    pub const fn new(storage: SqliteStorage) -> Self {
        Self { storage }
    }

    /// Returns the canonical database path for composition-root equality checks.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        self.storage.database_path()
    }

    /// Deterministically checkpoints and closes the Usage connection.
    ///
    /// # Errors
    ///
    /// Returns a bounded storage failure.
    pub fn close(self) -> Result<(), StorageError> {
        Box::new(self.storage).close()
    }
}

impl ArtifactEnterpriseUsagePort for DurableArtifactEnterpriseUsage {
    fn record_storage_source(
        &mut self,
        source: &ArtifactStorageSourceEntry,
    ) -> Result<EnterpriseUsageReceipt, EnterpriseUsageError> {
        self.storage
            .enterprise_usage_ledger()?
            .record(&storage_usage_fact(source))
    }
}

/// A reservation returned only after Artifact open admission succeeded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactEnterpriseQuotaReservation {
    receipt: EnterpriseQuotaReservationReceipt,
}

impl ArtifactEnterpriseQuotaReservation {
    /// Returns the durable reservation receipt needed for release or settlement.
    #[must_use]
    pub const fn receipt(&self) -> &EnterpriseQuotaReservationReceipt {
        &self.receipt
    }
}

/// Enterprise admission outcome for one exact Artifact open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactEnterpriseQuotaAdmission {
    Admitted(ArtifactEnterpriseQuotaReservation),
    TerminalReplay(EnterpriseQuotaReservationReceipt),
    Denied(EnterpriseQuotaDenial),
}

/// Failure from Artifact quota reservation, usage recording, or settlement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactEnterpriseQuotaSagaError {
    Artifact(ArtifactError),
    Quota(EnterpriseQuotaError),
    Usage(EnterpriseUsageError),
    Storage(StorageError),
    MissingFinalStorageSource,
    UnexpectedTerminalReservation,
}

impl fmt::Display for ArtifactEnterpriseQuotaSagaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Artifact(_) => formatter.write_str("Artifact quota source lookup failed"),
            Self::Quota(_) => formatter.write_str("Artifact enterprise quota operation failed"),
            Self::Usage(_) => formatter.write_str("Artifact enterprise Usage recording failed"),
            Self::Storage(_) => {
                formatter.write_str("Artifact enterprise quota time authority is invalid")
            }
            Self::MissingFinalStorageSource => {
                formatter.write_str("Artifact finalization has no immutable storage Usage source")
            }
            Self::UnexpectedTerminalReservation => formatter
                .write_str("Artifact metadata and its enterprise quota terminal state disagree"),
        }
    }
}

impl std::error::Error for ArtifactEnterpriseQuotaSagaError {}

impl From<ArtifactError> for ArtifactEnterpriseQuotaSagaError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(error)
    }
}

impl From<EnterpriseQuotaError> for ArtifactEnterpriseQuotaSagaError {
    fn from(error: EnterpriseQuotaError) -> Self {
        Self::Quota(error)
    }
}

impl From<EnterpriseUsageError> for ArtifactEnterpriseQuotaSagaError {
    fn from(error: EnterpriseUsageError) -> Self {
        Self::Usage(error)
    }
}

impl From<StorageError> for ArtifactEnterpriseQuotaSagaError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// Coordinates the immutable Artifact source with enterprise allowance.
///
/// The quota and Usage ports are explicit so the composition root can prove
/// they target the same canonical Control Plane database. A missing or delayed
/// final Usage source fails closed; the saga never accepts Worker-reported
/// storage quantities.
pub struct ArtifactEnterpriseQuotaSaga<'quota, 'usage> {
    quota: &'quota mut dyn EnterpriseQuotaAdmissionPort,
    usage: &'usage mut dyn ArtifactEnterpriseUsagePort,
}

impl<'quota, 'usage> ArtifactEnterpriseQuotaSaga<'quota, 'usage> {
    /// Binds the one enterprise quota admission port and immutable Usage recorder.
    #[must_use]
    pub fn new(
        quota: &'quota mut dyn EnterpriseQuotaAdmissionPort,
        usage: &'usage mut dyn ArtifactEnterpriseUsagePort,
    ) -> Self {
        Self { quota, usage }
    }

    /// Reserves exact storage allowance before the Artifact metadata/object write.
    ///
    /// `requested_at` must be the lease-validated open timestamp, not a new
    /// caller-selected time. Attribution, Artifact identity, open request, and
    /// expected bytes are read from the frozen [`ArtifactOpen`].
    ///
    /// # Errors
    ///
    /// Fails closed for malformed frozen authority, quota storage failure, or a
    /// changed replay.
    pub fn reserve_open(
        &mut self,
        open: &ArtifactOpen,
        requested_at: &Instant,
    ) -> Result<ArtifactEnterpriseQuotaAdmission, ArtifactEnterpriseQuotaSagaError> {
        let request = EnterpriseQuotaReservationRequest {
            reservation_id: open.request_id().clone(),
            attribution: usage_attribution(open.metering_attribution()),
            source_seal: EnterpriseQuotaSourceSeal::Storage {
                artifact_id: open.artifact_id().clone(),
                operation_kind: ArtifactStorageOperationKind::ArtifactFinalize,
                request_id: open.request_id().clone(),
                expected_bytes: open.size_bytes(),
            },
            reserved: EnterpriseQuotaAmounts {
                storage_bytes: open.size_bytes(),
                operations: 1,
                ..EnterpriseQuotaAmounts::default()
            },
            requested_at: requested_at.clone(),
        };
        Ok(match self.quota.reserve(&request)? {
            EnterpriseQuotaAdmission::Admitted(permit) => {
                ArtifactEnterpriseQuotaAdmission::Admitted(ArtifactEnterpriseQuotaReservation {
                    receipt: permit.receipt().clone(),
                })
            }
            EnterpriseQuotaAdmission::TerminalReplay(receipt) => {
                ArtifactEnterpriseQuotaAdmission::TerminalReplay(*receipt)
            }
            EnterpriseQuotaAdmission::Denied(denial) => {
                ArtifactEnterpriseQuotaAdmission::Denied(denial)
            }
        })
    }

    /// Releases an active reservation after a cancelled, failed, or rejected write.
    ///
    /// The release command identity is the durable open request, so retry and
    /// process restart replay the same terminal operation exactly.
    ///
    /// # Errors
    ///
    /// Fails closed for stale revisions, changed terminal replay, or unavailable
    /// quota storage.
    pub fn release(
        &mut self,
        reservation: &ArtifactEnterpriseQuotaReservation,
        reason: EnterpriseQuotaReleaseReason,
        released_at: &Instant,
    ) -> Result<EnterpriseQuotaReservationReceipt, ArtifactEnterpriseQuotaSagaError> {
        Ok(self.quota.release(&EnterpriseQuotaRelease {
            reservation_id: reservation.receipt.record.reservation_id.clone(),
            request_id: reservation.receipt.record.reservation_id.clone(),
            expected_revision: reservation.receipt.record.revision,
            reason,
            released_at: released_at.clone(),
        })?)
    }

    /// Records the Artifact finalization source and settles the exact reservation.
    ///
    /// The source is looked up only from the Artifact catalog after finalization.
    /// This keeps Worker-provided bytes, attribution, and operation identity out
    /// of the settlement path.
    ///
    /// # Errors
    ///
    /// Fails closed until the immutable final storage source exists, then rejects
    /// any source, attribution, byte, or revision mismatch.
    pub fn settle_final(
        &mut self,
        reservation: &ArtifactEnterpriseQuotaReservation,
        artifacts: &ArtifactStore,
    ) -> Result<EnterpriseQuotaReservationReceipt, ArtifactEnterpriseQuotaSagaError> {
        let EnterpriseQuotaSourceSeal::Storage { artifact_id, .. } =
            &reservation.receipt.record.source_seal
        else {
            unreachable!("Artifact quota reservation always uses a storage source seal");
        };
        let source = artifacts
            .storage_source_for_artifact(artifact_id)?
            .ok_or(ArtifactEnterpriseQuotaSagaError::MissingFinalStorageSource)?;
        let usage = self.usage.record_storage_source(&source)?;
        Ok(self.quota.settle(&EnterpriseQuotaSettlement {
            reservation_id: reservation.receipt.record.reservation_id.clone(),
            request_id: reservation.receipt.record.reservation_id.clone(),
            expected_revision: reservation.receipt.record.revision,
            usage_source: usage.entry.fact.source,
        })?)
    }

    /// Projects and settles a completed Artifact after a process restart.
    ///
    /// The Artifact catalog and enterprise Usage ledger provide all identity;
    /// callers supply only the Artifact id and cannot change attribution or bytes.
    ///
    /// # Errors
    ///
    /// Fails closed for a missing final source, a Usage conflict, or a matching
    /// released/corrupt quota reservation.
    pub fn recover_final(
        &mut self,
        artifact_id: &ArtifactId,
        artifacts: &ArtifactStore,
    ) -> Result<Option<EnterpriseQuotaReservationReceipt>, ArtifactEnterpriseQuotaSagaError> {
        let source = artifacts
            .storage_source_for_artifact(artifact_id)?
            .ok_or(ArtifactEnterpriseQuotaSagaError::MissingFinalStorageSource)?;
        let usage = self.usage.record_storage_source(&source)?;
        Ok(self.quota.settle_usage_source(&usage.entry.fact.source)?)
    }

    /// Releases every unfinished Artifact reservation sealed to one terminal Job.
    ///
    /// The catalog provides a bounded list of exact immutable opens. An exact
    /// retry accepts only the original active reservation or its matching
    /// released terminal receipt; a settled or newly denied state contradicts
    /// an unfinished Artifact and fails closed.
    ///
    /// # Errors
    ///
    /// Rejects corrupt/over-bound catalog authority, changed quota facts,
    /// settled unfinished Artifacts, or unavailable durable storage.
    pub fn release_unfinished_job(
        &mut self,
        artifacts: &ArtifactStore,
        execution_job_id: &ExecutionJobId,
        reason: EnterpriseQuotaReleaseReason,
        released_at: &Instant,
    ) -> Result<Vec<EnterpriseQuotaReservationReceipt>, ArtifactEnterpriseQuotaSagaError> {
        artifacts
            .unfinished_quota_opens_for_job(execution_job_id)?
            .into_iter()
            .map(|open| {
                let requested_at = crate::instant_from_millis(open.created_at_millis())?;
                match self.reserve_open(&open, &requested_at)? {
                    ArtifactEnterpriseQuotaAdmission::Admitted(reservation) => {
                        self.release(&reservation, reason, released_at)
                    }
                    ArtifactEnterpriseQuotaAdmission::TerminalReplay(receipt)
                        if receipt.record.state == EnterpriseQuotaReservationState::Released =>
                    {
                        Ok(receipt)
                    }
                    ArtifactEnterpriseQuotaAdmission::TerminalReplay(_)
                    | ArtifactEnterpriseQuotaAdmission::Denied(_) => {
                        Err(ArtifactEnterpriseQuotaSagaError::UnexpectedTerminalReservation)
                    }
                }
            })
            .collect()
    }
}

fn usage_attribution(
    source: &winwincode_storage::ArtifactMeteringAttribution,
) -> EnterpriseUsageAttribution {
    EnterpriseUsageAttribution {
        organization_id: source.organization_id.clone(),
        workspace_id: source.workspace_id.clone(),
        project_id: source.project_id.clone(),
        repository_id: source.repository_id.clone(),
        delivery_id: source.delivery_id.clone(),
        product_session_id: source.product_session_id.clone(),
        user_id: source.user_id.clone(),
    }
}

fn storage_usage_fact(source: &ArtifactStorageSourceEntry) -> SettledEnterpriseUsage {
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Storage {
            operation_id: source.fact.operation_id.clone(),
            source_sequence: source.sequence,
            source_digest: source.source_digest.clone(),
            artifact_id: source.fact.artifact_id.clone(),
            operation_kind: source.fact.operation_kind,
            request_id: source.fact.request_id.clone(),
        },
        attribution: usage_attribution(&source.fact.attribution),
        measure: EnterpriseUsageMeasure::Storage {
            bytes: source.fact.bytes,
        },
        settled_at: source.fact.occurred_at.clone(),
    }
}
