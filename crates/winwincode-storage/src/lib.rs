// SPDX-License-Identifier: Apache-2.0

//! Transactional product-state storage for the `WinWinCode` Control Plane.
//!
//! [`ProductStateStorage`] is the storage seam used by the Control Plane. A
//! commit replaces one canonical state value and atomically appends an optional
//! opaque aggregate-journal record, its scoped request receipt, and outbox
//! events. The interface deliberately does not expose transaction handles, so
//! callers cannot publish an event before every durable fact commits.

mod artifact;
mod client_registry;
mod control_plane_instances;
mod enterprise_policy;
mod enterprise_policy_evaluation;
mod enterprise_quota;
mod enterprise_usage;
mod enterprise_worker_placement;
mod execution_admission;
mod execution_queue;
mod execution_registry;
mod execution_scope_replacement;
mod git_candidate_retention;
mod git_source;
mod provider_exchange;
mod repository_scheduler;
mod repository_scheduler_replacement;
mod scheduler_policy;
mod worker_fleet_inventory;
mod worker_fleet_operations;
mod worker_outbound_queue;
mod worker_placement;
mod worker_registry;
mod worker_session_slots;

pub use artifact::{
    ArtifactAccess, ArtifactChunk, ArtifactError, ArtifactErrorKind, ArtifactMeteringAttribution,
    ArtifactObject, ArtifactObjectRange, ArtifactObjectStore, ArtifactOpen, ArtifactProvenance,
    ArtifactRangeObject, ArtifactRecord, ArtifactRetention, ArtifactStorageOperationKind,
    ArtifactStorageSourceCursor, ArtifactStorageSourceEntry, ArtifactStorageSourceFact,
    ArtifactStorageSourcePage, ArtifactStore, ArtifactWriteReceipt, FakeArtifactObjectStore,
    LocalArtifactObjectStore, MAX_ARTIFACT_RANGE_BYTES,
};
pub use client_registry::{
    ClientExchangeCursors, ClientLockState, ClientNodeRecord, ClientNodeRegistration,
    ClientNodeRegistrationReceipt, ClientNodeRegistry, ClientPresenceState, ClientRegistryError,
    ClientRegistryErrorKind,
};
pub use control_plane_instances::{
    ControlPlaneCommandAdmission, ControlPlaneCommandClaim, ControlPlaneCommittedCommand,
    ControlPlaneInstanceAuthority, ControlPlaneInstanceError, ControlPlaneInstanceErrorKind,
    ControlPlaneInstanceHealth, ControlPlaneInstanceIdentity, ControlPlaneInstanceLedger,
    ControlPlaneInstanceState,
};
pub use enterprise_policy::{
    EnterprisePolicyActor, EnterprisePolicyChildOverrideMode, EnterprisePolicyCursor,
    EnterprisePolicyDefinition, EnterprisePolicyEffect, EnterprisePolicyError,
    EnterprisePolicyErrorKind, EnterprisePolicyFilter, EnterprisePolicyInheritanceMode,
    EnterprisePolicyKind, EnterprisePolicyLedger, EnterprisePolicyMode, EnterprisePolicyPage,
    EnterprisePolicyRule, EnterprisePolicyScope, EnterprisePolicyState, EnterprisePolicyVersion,
    EnterprisePolicyVersionReference, EnterprisePolicyVersionSource, EnterprisePolicyWrite,
    EnterprisePolicyWriteReceipt,
};
pub use enterprise_policy_evaluation::{
    EnterprisePolicyEvaluation, EnterprisePolicyEvaluationAudit,
    EnterprisePolicyEvaluationAuditCursor, EnterprisePolicyEvaluationAuditPage,
    EnterprisePolicyEvaluationCommand, EnterprisePolicyEvaluationError,
    EnterprisePolicyEvaluationErrorKind, EnterprisePolicyEvaluationInput,
    EnterprisePolicyEvaluationLedger, EnterprisePolicyEvaluationOutcome,
    EnterprisePolicyEvaluationReason, EnterprisePolicyEvaluationReceipt,
    EnterprisePolicyEvaluationRequest, EnterprisePolicyExceptionDecision,
    EnterprisePolicyExceptionDecisionCommand, EnterprisePolicyExceptionId,
    EnterprisePolicyExceptionReceipt, EnterprisePolicyExceptionReference,
    EnterprisePolicyExceptionRequest, EnterprisePolicyExceptionState,
    EnterprisePolicyExceptionVersion,
};
pub use enterprise_quota::{
    EnterpriseQuotaAmounts, EnterpriseQuotaBoundary, EnterpriseQuotaDecision,
    EnterpriseQuotaDenial, EnterpriseQuotaDimension, EnterpriseQuotaError,
    EnterpriseQuotaErrorKind, EnterpriseQuotaLedger, EnterpriseQuotaLimits, EnterpriseQuotaPolicy,
    EnterpriseQuotaPolicyReceipt, EnterpriseQuotaPolicySeal, EnterpriseQuotaRelease,
    EnterpriseQuotaReleaseReason, EnterpriseQuotaReservationReceipt,
    EnterpriseQuotaReservationRecord, EnterpriseQuotaReservationRequest,
    EnterpriseQuotaReservationState, EnterpriseQuotaSettlement, EnterpriseQuotaSourceSeal,
    EnterpriseQuotaTerminal,
};
pub use enterprise_usage::{
    EnterpriseUsageAttribution, EnterpriseUsageCursor, EnterpriseUsageEntry, EnterpriseUsageError,
    EnterpriseUsageErrorKind, EnterpriseUsageFilter, EnterpriseUsageLedger, EnterpriseUsageMeasure,
    EnterpriseUsagePage, EnterpriseUsageReceipt, EnterpriseUsageSource, EnterpriseUsageSourceKind,
    EnterpriseUsageTotals, SettledEnterpriseUsage,
};
pub use enterprise_worker_placement::{
    EnterpriseWorkerLeaseClaim, EnterpriseWorkerPlacementCandidate,
    EnterpriseWorkerPlacementDecision, EnterpriseWorkerPlacementRequest,
    EnterpriseWorkerPlacementSelection, EnterpriseWorkerPoolProfile, EnterpriseWorkerSecurityTier,
    claim_enterprise_worker_selection, place_enterprise_worker_batch,
};
pub use execution_admission::{
    ExecutionAdmission, ExecutionAdmissionBoundary, ExecutionAdmissionError,
    ExecutionAdmissionErrorCode, ExecutionAdmissionLimits, ExecutionAdmissionPolicy,
    ExecutionAdmissionReceipt, ExecutionAdmissionUsage, ExecutionRepositoryAccess,
    ExecutionReservationRecord, ExecutionReservationRelease, ExecutionReservationReleaseReason,
    ExecutionReservationRequest, ExecutionReservationSettlement, ExecutionReservationStart,
    ExecutionReservationState, WorkerPoolId, WorkerSettlementSourceCursor,
    WorkerSettlementSourceEntry, WorkerSettlementSourceFact, WorkerSettlementSourcePage,
};
pub use execution_queue::{
    ExecutionJobCancellationIntent, ExecutionJobCancellationRequest, ExecutionJobMutationReceipt,
    ExecutionJobPage, ExecutionJobPageCursor, ExecutionJobRecord, ExecutionJobState,
    ExecutionJobSubmission, ExecutionJobTransitionRequest, ExecutionQueue, ExecutionQueueScope,
};
pub use execution_registry::{
    ActiveLeaseSummary, AuthenticatedWorkerPlacement, DispatchResultError, DispatchResultErrorCode,
    DispatchResultReceipt, DispatchResultRequest, DispatchResultStatus, ExecutionDispatchAuthority,
    ExecutionLeaseClaim, ExecutionLeasePlacement, ExecutionLeaseReceipt, ExecutionLeaseRecord,
    ExecutionLeaseRenewal, ExecutionLeaseTerminalOutcome, ExecutionLeaseTerminalRequest,
    ExecutionRegistry, LeaseRecovery, LeaseWriteStatus, WorkerHeartbeatReceipt,
    WorkerHeartbeatRequest, WorkerManagementCommand, WorkerManagementReceipt, WorkerRecord,
    WorkerRegistrationReceipt, WorkerRegistrationRequest, WorkerRegistrationStatus,
};
pub use execution_scope_replacement::ExecutionScopeReplacementAuthority;
pub use git_candidate_retention::{
    CandidateGitPinReceipt, CandidateGitReleaseAuthority, CandidateGitReleaseReceipt,
    CandidateGitRetention, CandidateGitRetentionError, CandidateGitRetentionErrorKind,
    CandidateGitRetentionState, CandidateGitTerminalOutcome,
};
pub use git_source::{
    CandidateSourceManifest, GitCandidateReviewFile, GitCandidateReviewFileEncoding,
    GitCandidateReviewFileStatus, GitSourceHunk, GitSourcePath, GitSourcePathState,
    GitSourceResolver, LocalGitSourceResolver, ValidatedGitCandidateDiff,
    ValidatedGitCandidateReview, ValidatedGitSourceArtifact,
};
pub use provider_exchange::{
    ModelRequestPoolAuthority, ProviderExchangeBegin, ProviderExchangeFailure,
    ProviderExchangeFinalAck, ProviderExchangeOpened, ProviderExchangeSnapshot,
    ProviderExchangeState, ProviderExchangeStore, ProviderExchangeStoreError,
    ProviderExchangeStoreErrorCode, ProviderExchangeTerminal, ProviderExchangeTerminalProgress,
    ProviderExchangeTerminalStage,
};
pub use repository_scheduler::{
    RepositoryScheduler, RepositorySchedulerCancellationReceipt,
    RepositorySchedulerCancellationRequest, RepositorySchedulerClaimReceipt,
    RepositorySchedulerClaimRequest, RepositorySchedulerDispatchResultReceipt,
    RepositorySchedulerDispatchResultRequest, RepositorySchedulerRetryRequest,
    RepositorySchedulerScope, RepositorySchedulerTerminalReceipt,
    RepositorySchedulerTerminalRequest,
};
pub use scheduler_policy::{
    SchedulerCancellationPlan, SchedulerCancellationTarget, SchedulerCandidate, SchedulerDispatch,
    SchedulerPolicy, SchedulerPolicyError, SchedulerPriority, SchedulerRetryDecision,
    SchedulerRetryPolicy, SchedulerWeights, plan_scheduler_cancellation, scheduler_retry_decision,
};
pub use worker_fleet_inventory::{
    WorkerFleetInventoryPage, WorkerFleetInventoryState, WorkerFleetInventoryStore,
    WorkerFleetPoolInventory, WorkerFleetSnapshotCursor, WorkerFleetSnapshotRequest,
};
pub use worker_fleet_operations::{
    WorkerFleetAction, WorkerFleetFailureCommand, WorkerFleetFailureReceipt,
    WorkerFleetFencedLease, WorkerFleetMemberHealth, WorkerFleetMemberObservation,
    WorkerFleetObservation, WorkerFleetOperations, WorkerFleetPendingReplacement,
    WorkerFleetReleaseVersion, WorkerFleetRolloutCommand, WorkerFleetRolloutPhase,
    WorkerFleetRolloutPolicy, WorkerFleetRolloutReceipt, WorkerFleetRolloutRecord,
};
pub use worker_outbound_queue::{
    WorkerOutboundAcknowledgement, WorkerOutboundAuthority, WorkerOutboundClaim,
    WorkerOutboundClaimPage, WorkerOutboundEnqueueReceipt, WorkerOutboundEnqueueRequest,
    WorkerOutboundMessageState, WorkerOutboundPageCursor, WorkerOutboundQueue,
    WorkerOutboundQueueConfig, WorkerOutboundQueueError, WorkerOutboundQueueErrorCode,
    WorkerOutboundSettlement,
};
pub use worker_placement::{
    WorkerAffinityFailure, WorkerPlacementCandidate, WorkerPlacementCandidateRejection,
    WorkerPlacementDecision, WorkerPlacementError, WorkerPlacementFailure,
    WorkerPlacementGlobalFailure, WorkerPlacementQuota, WorkerPlacementRejection,
    WorkerPlacementRequest, WorkerPlacementSelection, WorkerRepositoryAccess,
    WorkerSessionAffinity, place_worker_batch,
};
pub use worker_registry::{
    EXECUTION_PROTOCOL_VERSION, WorkerAuthenticationIdentity, WorkerCapacityEntry,
    WorkerCapacitySnapshot, WorkerHealth, WorkerManagementPage, WorkerManagementPageCursor,
    WorkerManagementSnapshot, WorkerManagementState, WorkerOperationalState, WorkerPlatform,
    WorkerRegistrationErrorCode, WorkerRegistryScope,
};
pub use worker_session_slots::{
    WorkerSessionSlots, WorkerSlotAuthority, WorkerSlotCancellation, WorkerSlotCapacity,
    WorkerSlotCloseRequest, WorkerSlotError, WorkerSlotErrorCode, WorkerSlotEventAdvance,
    WorkerSlotOpenRequest, WorkerSlotReceipt, WorkerSlotRecord, WorkerSlotRecoveryAction,
    WorkerSlotRecoveryReceipt, WorkerSlotRecoveryRequest, WorkerSlotResourceLimits,
    WorkerSlotResources, WorkerSlotState,
};

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError, Weak};
use std::thread;
use std::time::{Duration, Instant as StdInstant};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    CodexThreadId, ControlPlaneEventId, DeliveryId, ExecutionJobId, Instant, LeaseId,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, ServiceAccountId,
    SessionIdentity, Sha256Digest, SystemActorId, UserId, WorkerId, WorkerSessionId, WorkspaceId,
    is_canonical_delivery_id,
};

const DATABASE_FILE_NAME: &str = "control-plane.sqlite3";
const SCHEMA_VERSION: i64 = 6;
static SQLITE_OPEN_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
// SQLite's default busy timeout accumulates requested sleeps and is not a wall-clock deadline.
// The open-only handler reads this thread-local monotonic deadline on every busy callback.
thread_local! {
    static SQLITE_OPEN_BUSY_DEADLINE: Cell<Option<StdInstant>> = const { Cell::new(None) };
}
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(1);
const ACTOR_KEY_PREFIX: &[u8] = b"winwincode.command-receipt.actor.v1";
const SCOPE_KEY_PREFIX: &[u8] = b"winwincode.command-receipt.scope.v1";
const LEGACY_V1_ACTOR_KEY: &[u8] = b"winwincode.command-receipt.actor.legacy-v1";
const LEGACY_V1_SCOPE_KEY: &[u8] = b"winwincode.command-receipt.scope.legacy-v1";

/// One event to append atomically with a canonical state change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewOutboxEvent {
    pub event_id: String,
    pub topic: String,
    pub payload: Vec<u8>,
    projection_stream: Option<ProjectionEventStream>,
    public_context: Option<PublicProjectionEventContext>,
}

/// Complete immutable envelope facts for one public projection event.
///
/// These closed storage types are stored beside the payload in the same state
/// transaction. The Server reads this value directly after a crash instead of
/// decoding receipt keys or inventing a time or source.
#[derive(Clone, Debug, PartialEq)]
pub struct PublicProjectionEventContext {
    scope: PublicEventScope,
    stream: ProjectionEventStream,
    occurred_at: Instant,
    source: PublicEventSource,
}

impl Eq for PublicProjectionEventContext {}

impl PublicProjectionEventContext {
    #[must_use]
    pub const fn scope(&self) -> &PublicEventScope {
        &self.scope
    }

    #[must_use]
    pub const fn stream(&self) -> &ProjectionEventStream {
        &self.stream
    }

    #[must_use]
    pub const fn occurred_at(&self) -> &Instant {
        &self.occurred_at
    }

    #[must_use]
    pub const fn source(&self) -> &PublicEventSource {
        &self.source
    }
}

/// Closed public actor identity stored without authentication material.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PublicEventActor {
    User { id: UserId },
    ServiceAccount { id: ServiceAccountId },
    System { id: SystemActorId },
}

/// Closed tenant scope stored with every public event.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PublicEventScope {
    Organization {
        organization_id: OrganizationId,
    },
    Workspace {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    },
    Project {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    },
    Repository {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        repository_id: RepositoryId,
    },
}

/// Closed secret-safe origin stored with every public event.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PublicEventSource {
    ControlPlane {
        actor: PublicEventActor,
        component: String,
    },
    ExecutionWorker {
        worker_id: WorkerId,
        worker_session_id: WorkerSessionId,
        lease_id: LeaseId,
        codex_thread_id: CodexThreadId,
    },
    SessionExecutionWorker {
        worker_id: WorkerId,
        worker_session_id: WorkerSessionId,
        lease_id: LeaseId,
        codex_thread_id: CodexThreadId,
        session_identity: SessionIdentity,
    },
}

impl Eq for PublicEventSource {}

/// Closed resource stream used to hand an HTTP snapshot to WebSocket replay.
#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum ProjectionEventStream {
    /// One ordered stream per exact tenant scope key.
    Scope,
    Delivery(DeliveryId),
    ProductSession(ProductSessionId),
    /// One ordered stream per exact `WorkerId` and `LeaseId` identity.
    Lease {
        worker_id: WorkerId,
        lease_id: LeaseId,
    },
}

impl ProjectionEventStream {
    fn kind(&self) -> &'static str {
        match self {
            Self::Scope => "scope",
            Self::Delivery(_) => "delivery",
            Self::ProductSession(_) => "product-session",
            Self::Lease { .. } => "lease",
        }
    }

    fn resource_id(&self) -> String {
        match self {
            Self::Scope => String::new(),
            Self::Delivery(delivery_id) => delivery_id.0.clone(),
            Self::ProductSession(product_session_id) => product_session_id.0.clone(),
            Self::Lease {
                worker_id,
                lease_id,
            } => format!("{}/{}", worker_id.0, lease_id.0),
        }
    }

    fn validate(&self) -> Result<(), StorageError> {
        let (prefix, valid) = match self {
            Self::Scope => ("scope", true),
            Self::Delivery(delivery_id) => ("delivery", is_canonical_delivery_id(&delivery_id.0)),
            Self::ProductSession(product_session_id) => {
                let value = product_session_id.0.as_str();
                (
                    "product-session",
                    !value.is_empty() && value.len() <= 200 && portable_event_key(value),
                )
            }
            Self::Lease {
                worker_id,
                lease_id,
            } => (
                "lease",
                require_canonical_public_id(&worker_id.0, "wrk_", "lease stream workerId").is_ok()
                    && require_canonical_public_id(&lease_id.0, "lse_", "lease stream leaseId")
                        .is_ok(),
            ),
        };
        if !valid {
            return Err(StorageError::invalid(format!(
                "{prefix} event stream identity is invalid"
            )));
        }
        Ok(())
    }

    fn from_stored(kind: &str, resource_id: String) -> Result<Self, StorageError> {
        let stream = match kind {
            "scope" if resource_id.is_empty() => Self::Scope,
            "delivery" => Self::Delivery(DeliveryId(resource_id)),
            "product-session" => Self::ProductSession(ProductSessionId(resource_id)),
            "lease" => {
                let Some((worker_id, lease_id)) = resource_id.split_once('/') else {
                    return Err(StorageError::adapter(
                        "stored lease event stream identity is invalid",
                    ));
                };
                if lease_id.contains('/') {
                    return Err(StorageError::adapter(
                        "stored lease event stream identity is invalid",
                    ));
                }
                Self::Lease {
                    worker_id: WorkerId(worker_id.to_owned()),
                    lease_id: LeaseId(lease_id.to_owned()),
                }
            }
            _ => return Err(StorageError::adapter("stored event stream kind is invalid")),
        };
        stream.validate()?;
        Ok(stream)
    }
}

fn validate_public_event_time(occurred_at: &Instant) -> Result<(), StorageError> {
    let bytes = occurred_at.0.as_bytes();
    let punctuation = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'.'),
    ];
    if bytes.len() != 24
        || bytes[23] != b'Z'
        || punctuation
            .iter()
            .any(|(index, byte)| bytes[*index] != *byte)
        || bytes.iter().enumerate().any(|(index, byte)| {
            !punctuation.iter().any(|(at, _)| at == &index) && index != 23 && !byte.is_ascii_digit()
        })
    {
        return Err(StorageError::invalid("public event occurredAt is invalid"));
    }
    Ok(())
}

fn validate_public_source(source: &PublicEventSource) -> Result<(), StorageError> {
    match source {
        PublicEventSource::ControlPlane { actor, component } => {
            if component.trim().is_empty() {
                return Err(StorageError::invalid("public event component is invalid"));
            }
            receipt_actor_key(actor)?;
        }
        PublicEventSource::ExecutionWorker {
            worker_id,
            worker_session_id,
            lease_id,
            codex_thread_id,
        } => {
            require_canonical_public_id(&worker_id.0, "wrk_", "source workerId")?;
            require_canonical_public_id(&worker_session_id.0, "wsn_", "source workerSessionId")?;
            require_canonical_public_id(&lease_id.0, "lse_", "source leaseId")?;
            require_canonical_public_id(&codex_thread_id.0, "cdx_", "source codexThreadId")?;
        }
        PublicEventSource::SessionExecutionWorker {
            worker_id,
            worker_session_id,
            lease_id,
            codex_thread_id,
            session_identity,
        } => {
            if worker_session_id != &session_identity.worker_session_id
                || codex_thread_id != &session_identity.codex_thread_id
            {
                return Err(StorageError::invalid(
                    "public Worker source differs from sessionIdentity",
                ));
            }
            require_canonical_public_id(&worker_id.0, "wrk_", "source workerId")?;
            require_canonical_public_id(&worker_session_id.0, "wsn_", "source workerSessionId")?;
            require_canonical_public_id(&lease_id.0, "lse_", "source leaseId")?;
            require_canonical_public_id(&codex_thread_id.0, "cdx_", "source codexThreadId")?;
            require_canonical_public_id(
                &session_identity.product_session_id.0,
                "psn_",
                "source productSessionId",
            )?;
            if let Some(stage_run_id) = &session_identity.stage_run_id {
                require_canonical_public_id(&stage_run_id.0, "run_", "source stageRunId")?;
            }
        }
    }
    Ok(())
}

fn validate_public_stream_source(
    stream: &ProjectionEventStream,
    source: &PublicEventSource,
) -> Result<(), StorageError> {
    let ProjectionEventStream::Lease {
        worker_id: stream_worker_id,
        lease_id: stream_lease_id,
    } = stream
    else {
        return Ok(());
    };
    let worker_authority = match source {
        PublicEventSource::ControlPlane { .. } => return Ok(()),
        PublicEventSource::ExecutionWorker {
            worker_id,
            lease_id,
            ..
        }
        | PublicEventSource::SessionExecutionWorker {
            worker_id,
            lease_id,
            ..
        } => (worker_id, lease_id),
    };
    if worker_authority != (stream_worker_id, stream_lease_id) {
        return Err(StorageError::invalid(
            "Lease event stream differs from its Worker source authority",
        ));
    }
    Ok(())
}

/// Exact tenant scope and resource stream key owned by durable storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionEventStreamKey {
    scope_key: ReceiptScopeKey,
    stream: ProjectionEventStream,
}

impl ProjectionEventStreamKey {
    /// Creates one exact event stream key.
    ///
    /// # Errors
    ///
    /// Rejects an invalid resource identity.
    pub fn new(
        scope_key: ReceiptScopeKey,
        stream: ProjectionEventStream,
    ) -> Result<Self, StorageError> {
        stream.validate()?;
        Ok(Self { scope_key, stream })
    }

    #[must_use]
    pub const fn scope_key(&self) -> &ReceiptScopeKey {
        &self.scope_key
    }

    #[must_use]
    pub const fn stream(&self) -> &ProjectionEventStream {
        &self.stream
    }
}

/// Last retained event included in one complete HTTP snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionEventCursor {
    key: ProjectionEventStreamKey,
    sequence: u64,
    event_id: Option<ControlPlaneEventId>,
}

impl ProjectionEventCursor {
    /// Rebuilds an exact cursor supplied by the typed application service.
    ///
    /// # Errors
    ///
    /// Rejects a sequence/event identity mismatch or unsafe public value.
    pub fn try_new(
        key: ProjectionEventStreamKey,
        sequence: u64,
        event_id: Option<ControlPlaneEventId>,
    ) -> Result<Self, StorageError> {
        if sequence > 9_007_199_254_740_991
            || (sequence == 0) != event_id.is_none()
            || event_id
                .as_ref()
                .is_some_and(|event_id| !canonical_control_plane_event_id(&event_id.0))
        {
            return Err(StorageError::adapter(
                "stored projection event cursor is invalid",
            ));
        }
        Ok(Self {
            key,
            sequence,
            event_id,
        })
    }

    #[must_use]
    pub const fn key(&self) -> &ProjectionEventStreamKey {
        &self.key
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn event_id(&self) -> Option<&ControlPlaneEventId> {
        self.event_id.as_ref()
    }
}

/// Stable opaque identity for one append-only domain journal.
///
/// Storage treats both fields as bound data. The Control Plane adapter owns
/// their domain meaning, so this crate does not depend on Delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AggregateJournalKey {
    aggregate_type: String,
    aggregate_id: String,
}

impl AggregateJournalKey {
    /// Builds one opaque aggregate journal identity.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::InvalidInput`] when either component is empty.
    pub fn new(
        aggregate_type: impl Into<String>,
        aggregate_id: impl Into<String>,
    ) -> Result<Self, StorageError> {
        let aggregate_type = aggregate_type.into();
        let aggregate_id = aggregate_id.into();
        if aggregate_type.is_empty() || aggregate_id.is_empty() {
            return Err(StorageError::invalid(
                "aggregate journal type and id must not be empty",
            ));
        }
        Ok(Self {
            aggregate_type,
            aggregate_id,
        })
    }

    #[must_use]
    pub fn aggregate_type(&self) -> &str {
        &self.aggregate_type
    }

    #[must_use]
    pub fn aggregate_id(&self) -> &str {
        &self.aggregate_id
    }
}

/// One opaque, digest-addressed append-only journal record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AggregateJournalRecord {
    pub sequence: u64,
    pub digest: String,
    pub payload: Vec<u8>,
}

impl AggregateJournalRecord {
    #[must_use]
    pub fn new(sequence: u64, digest: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            sequence,
            digest: digest.into(),
            payload: payload.into(),
        }
    }
}

/// Fully committed opaque journal bytes loaded through the storage port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedAggregateJournal {
    pub manifest: Vec<u8>,
    pub records: Vec<AggregateJournalRecord>,
}

/// One journal create or tail-CAS append staged in a state transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AggregateJournalPublication {
    Create {
        key: AggregateJournalKey,
        manifest: Vec<u8>,
        first_record: AggregateJournalRecord,
    },
    Append {
        key: AggregateJournalKey,
        expected_tail_sequence: u64,
        expected_tail_digest: String,
        record: AggregateJournalRecord,
    },
}

impl AggregateJournalPublication {
    fn validate(&self) -> Result<(), StorageError> {
        match self {
            Self::Create {
                manifest,
                first_record,
                ..
            } => {
                if manifest.is_empty() {
                    return Err(StorageError::invalid(
                        "aggregate journal manifest must not be empty",
                    ));
                }
                validate_journal_record(first_record)?;
                if first_record.sequence != 1 {
                    return Err(StorageError::invalid(
                        "aggregate journal first record sequence must be 1",
                    ));
                }
            }
            Self::Append {
                expected_tail_sequence,
                expected_tail_digest,
                record,
                ..
            } => {
                validate_journal_record(record)?;
                if *expected_tail_sequence == 0 || expected_tail_digest.is_empty() {
                    return Err(StorageError::invalid(
                        "aggregate journal expected tail must be complete",
                    ));
                }
                if record.sequence != expected_tail_sequence.saturating_add(1) {
                    return Err(StorageError::invalid(
                        "aggregate journal append sequence must follow the expected tail",
                    ));
                }
            }
        }
        Ok(())
    }
}

fn validate_journal_record(record: &AggregateJournalRecord) -> Result<(), StorageError> {
    if record.sequence == 0 || record.sequence > i64::MAX as u64 {
        return Err(StorageError::invalid(
            "aggregate journal sequence is outside the SQLite range",
        ));
    }
    if record.digest.is_empty() || record.payload.is_empty() {
        return Err(StorageError::invalid(
            "aggregate journal digest and payload must not be empty",
        ));
    }
    Ok(())
}

impl NewOutboxEvent {
    /// Creates an internal event that is never used as a public replay cursor.
    #[must_use]
    pub fn internal(
        event_id: impl Into<String>,
        topic: impl Into<String>,
        payload: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            topic: topic.into(),
            payload: payload.into(),
            projection_stream: None,
            public_context: None,
        }
    }

    /// Creates the only canonical public event shape with its complete durable
    /// envelope context.
    ///
    /// # Errors
    ///
    /// Rejects an invalid stream identity, occurrence time, or source before
    /// the state transaction. A Worker-origin Lease event must name the exact
    /// `WorkerId` and `LeaseId` authority carried by its source.
    pub fn public_projection(
        event_id: ControlPlaneEventId,
        topic: impl Into<String>,
        payload: impl Into<Vec<u8>>,
        stream: ProjectionEventStream,
        scope: PublicEventScope,
        occurred_at: Instant,
        source: PublicEventSource,
    ) -> Result<Self, StorageError> {
        stream.validate()?;
        receipt_scope_key(&scope)?;
        validate_public_event_time(&occurred_at)?;
        validate_public_source(&source)?;
        validate_public_stream_source(&stream, &source)?;
        Ok(Self {
            event_id: event_id.0,
            topic: topic.into(),
            payload: payload.into(),
            projection_stream: Some(stream.clone()),
            public_context: Some(PublicProjectionEventContext {
                scope,
                stream,
                occurred_at,
                source,
            }),
        })
    }

    #[must_use]
    pub const fn projection_stream(&self) -> Option<&ProjectionEventStream> {
        self.projection_stream.as_ref()
    }

    #[must_use]
    pub const fn public_context(&self) -> Option<&PublicProjectionEventContext> {
        self.public_context.as_ref()
    }
}

/// Opaque canonical actor identity encoded by the Control Plane adapter.
///
/// The storage adapter never receives authentication proof or credentials. It
/// only receives the stable actor identity fields from `CommandEnvelope`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiptActorKey(Vec<u8>);

impl ReceiptActorKey {
    /// Builds a typed storage key from the canonical actor fields.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::InvalidInput`] for an empty encoding.
    pub fn from_encoded(encoded: Vec<u8>) -> Result<Self, StorageError> {
        if encoded.is_empty() {
            return Err(StorageError::invalid("receipt actor key must not be empty"));
        }
        Ok(Self(encoded))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Opaque canonical organization/workspace/project/repository scope encoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiptScopeKey(Vec<u8>);

impl ReceiptScopeKey {
    /// Builds a typed storage key from every canonical scope field.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::InvalidInput`] for an empty encoding.
    pub fn from_encoded(encoded: Vec<u8>) -> Result<Self, StorageError> {
        if encoded.is_empty() {
            return Err(StorageError::invalid("receipt scope key must not be empty"));
        }
        Ok(Self(encoded))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// The complete idempotency identity of one canonical HTTP command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiptIdentity {
    actor_key: ReceiptActorKey,
    scope_key: ReceiptScopeKey,
    request_id: RequestId,
}

impl ReceiptIdentity {
    /// Combines actor, full scope, and request id into one receipt identity.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::InvalidInput`] for an empty request id.
    pub fn new(
        actor_key: ReceiptActorKey,
        scope_key: ReceiptScopeKey,
        request_id: RequestId,
    ) -> Result<Self, StorageError> {
        if request_id.0.is_empty() {
            return Err(StorageError::invalid("request_id must not be empty"));
        }
        Ok(Self {
            actor_key,
            scope_key,
            request_id,
        })
    }

    #[must_use]
    pub const fn actor_key(&self) -> &ReceiptActorKey {
        &self.actor_key
    }

    #[must_use]
    pub const fn scope_key(&self) -> &ReceiptScopeKey {
        &self.scope_key
    }

    #[must_use]
    pub const fn request_id(&self) -> &RequestId {
        &self.request_id
    }
}

/// Encodes the sole canonical durable actor key for a public actor.
///
/// # Errors
///
/// Returns [`StorageErrorKind::InvalidInput`] when the actor id is not
/// canonical.
pub fn receipt_actor_key(actor: &PublicEventActor) -> Result<ReceiptActorKey, StorageError> {
    let (tag, id, prefix) = match actor {
        PublicEventActor::User { id } => (b"user".as_slice(), id.0.as_str(), "usr_"),
        PublicEventActor::ServiceAccount { id } => {
            (b"service_account".as_slice(), id.0.as_str(), "svc_")
        }
        PublicEventActor::System { id } => (b"system".as_slice(), id.0.as_str(), "sys_"),
    };
    require_canonical_public_id(id, prefix, "actor id")?;
    ReceiptActorKey::from_encoded(encode_public_key(ACTOR_KEY_PREFIX, tag, &[id]))
}

/// Decodes the public actor stored in one canonical receipt actor key.
///
/// # Errors
///
/// Returns [`StorageErrorKind::InvalidInput`] when the opaque key is not a
/// canonical public actor encoding.
pub fn public_actor_from_receipt_key(
    key: &ReceiptActorKey,
) -> Result<PublicEventActor, StorageError> {
    let mut offset = 0;
    let prefix = decode_public_key_field(key.as_bytes(), &mut offset)?;
    let tag = decode_public_key_field(key.as_bytes(), &mut offset)?;
    let id = decode_public_key_field(key.as_bytes(), &mut offset)?;
    if offset != key.as_bytes().len() || prefix != ACTOR_KEY_PREFIX {
        return Err(StorageError::invalid(
            "receipt actor key is not a canonical public actor",
        ));
    }
    let id = String::from_utf8(id)
        .map_err(|_| StorageError::invalid("receipt actor id is not UTF-8"))?;
    match tag.as_slice() {
        b"user" => {
            require_canonical_public_id(&id, "usr_", "actor id")?;
            Ok(PublicEventActor::User { id: UserId(id) })
        }
        b"service_account" => {
            require_canonical_public_id(&id, "svc_", "actor id")?;
            Ok(PublicEventActor::ServiceAccount {
                id: ServiceAccountId(id),
            })
        }
        b"system" => {
            require_canonical_public_id(&id, "sys_", "actor id")?;
            Ok(PublicEventActor::System {
                id: SystemActorId(id),
            })
        }
        _ => Err(StorageError::invalid(
            "receipt actor key has an unknown public actor kind",
        )),
    }
}

/// Encodes the sole canonical durable scope key for a public scope.
///
/// # Errors
///
/// Returns [`StorageErrorKind::InvalidInput`] when any scope id is not
/// canonical.
pub fn receipt_scope_key(scope: &PublicEventScope) -> Result<ReceiptScopeKey, StorageError> {
    let encoded = match scope {
        PublicEventScope::Organization { organization_id } => {
            require_canonical_public_id(&organization_id.0, "org_", "organizationId")?;
            encode_public_key(
                SCOPE_KEY_PREFIX,
                b"organization",
                &[organization_id.0.as_str()],
            )
        }
        PublicEventScope::Workspace {
            organization_id,
            workspace_id,
        } => {
            require_canonical_public_id(&organization_id.0, "org_", "organizationId")?;
            require_canonical_public_id(&workspace_id.0, "wsp_", "workspaceId")?;
            encode_public_key(
                SCOPE_KEY_PREFIX,
                b"workspace",
                &[organization_id.0.as_str(), workspace_id.0.as_str()],
            )
        }
        PublicEventScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => {
            require_canonical_public_id(&organization_id.0, "org_", "organizationId")?;
            require_canonical_public_id(&workspace_id.0, "wsp_", "workspaceId")?;
            require_canonical_public_id(&project_id.0, "prj_", "projectId")?;
            encode_public_key(
                SCOPE_KEY_PREFIX,
                b"project",
                &[
                    organization_id.0.as_str(),
                    workspace_id.0.as_str(),
                    project_id.0.as_str(),
                ],
            )
        }
        PublicEventScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => {
            require_canonical_public_id(&organization_id.0, "org_", "organizationId")?;
            require_canonical_public_id(&workspace_id.0, "wsp_", "workspaceId")?;
            require_canonical_public_id(&project_id.0, "prj_", "projectId")?;
            require_canonical_public_id(&repository_id.0, "rep_", "repositoryId")?;
            encode_public_key(
                SCOPE_KEY_PREFIX,
                b"repository",
                &[
                    organization_id.0.as_str(),
                    workspace_id.0.as_str(),
                    project_id.0.as_str(),
                    repository_id.0.as_str(),
                ],
            )
        }
    };
    ReceiptScopeKey::from_encoded(encoded)
}

/// Decodes the canonical repository scope used by Worker-origin producers that
/// receive only a previously verified durable receipt identity.
///
/// # Errors
///
/// Returns [`StorageErrorKind::InvalidInput`] when the opaque key is not the
/// canonical repository-scope encoding.
pub fn repository_scope_from_receipt_key(
    key: &ReceiptScopeKey,
) -> Result<PublicEventScope, StorageError> {
    let mut offset = 0;
    let prefix = decode_public_key_field(key.as_bytes(), &mut offset)?;
    let tag = decode_public_key_field(key.as_bytes(), &mut offset)?;
    let organization_id = decode_public_key_field(key.as_bytes(), &mut offset)?;
    let workspace_id = decode_public_key_field(key.as_bytes(), &mut offset)?;
    let project_id = decode_public_key_field(key.as_bytes(), &mut offset)?;
    let repository_id = decode_public_key_field(key.as_bytes(), &mut offset)?;
    if offset != key.as_bytes().len() || prefix != SCOPE_KEY_PREFIX || tag != b"repository" {
        return Err(StorageError::invalid(
            "receipt scope key is not a canonical repository scope",
        ));
    }
    let organization_id = String::from_utf8(organization_id)
        .map_err(|_| StorageError::invalid("repository organizationId is not UTF-8"))?;
    let workspace_id = String::from_utf8(workspace_id)
        .map_err(|_| StorageError::invalid("repository workspaceId is not UTF-8"))?;
    let project_id = String::from_utf8(project_id)
        .map_err(|_| StorageError::invalid("repository projectId is not UTF-8"))?;
    let repository_id = String::from_utf8(repository_id)
        .map_err(|_| StorageError::invalid("repository repositoryId is not UTF-8"))?;
    require_canonical_public_id(&organization_id, "org_", "organizationId")?;
    require_canonical_public_id(&workspace_id, "wsp_", "workspaceId")?;
    require_canonical_public_id(&project_id, "prj_", "projectId")?;
    require_canonical_public_id(&repository_id, "rep_", "repositoryId")?;
    Ok(PublicEventScope::Repository {
        organization_id: OrganizationId(organization_id),
        workspace_id: WorkspaceId(workspace_id),
        project_id: ProjectId(project_id),
        repository_id: RepositoryId(repository_id),
    })
}

/// Builds one HTTP command receipt identity through the canonical actor/scope
/// encoders owned by storage.
///
/// # Errors
///
/// Returns [`StorageErrorKind::InvalidInput`] when the actor, scope, or request
/// id is not canonical.
pub fn public_receipt_identity(
    actor: &PublicEventActor,
    scope: &PublicEventScope,
    request_id: RequestId,
) -> Result<ReceiptIdentity, StorageError> {
    require_canonical_public_id(&request_id.0, "req_", "requestId")?;
    ReceiptIdentity::new(
        receipt_actor_key(actor)?,
        receipt_scope_key(scope)?,
        request_id,
    )
}

fn require_canonical_public_id(value: &str, prefix: &str, label: &str) -> Result<(), StorageError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(StorageError::invalid(format!("{label} is not canonical")));
    };
    if suffix.len() != 26 || !suffix.bytes().all(is_crockford_base32) {
        return Err(StorageError::invalid(format!("{label} is not canonical")));
    }
    Ok(())
}

const fn is_crockford_base32(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9'
            | b'A'..=b'H'
            | b'J'
            | b'K'
            | b'M'
            | b'N'
            | b'P'..=b'T'
            | b'V'..=b'Z'
    )
}

fn encode_public_key(prefix: &[u8], tag: &[u8], values: &[&str]) -> Vec<u8> {
    let mut encoded = Vec::new();
    append_public_key_field(&mut encoded, prefix);
    append_public_key_field(&mut encoded, tag);
    for value in values {
        append_public_key_field(&mut encoded, value.as_bytes());
    }
    encoded
}

fn append_public_key_field(encoded: &mut Vec<u8>, value: &[u8]) {
    encoded.extend_from_slice(&(value.len() as u64).to_be_bytes());
    encoded.extend_from_slice(value);
}

fn decode_public_key_field(encoded: &[u8], offset: &mut usize) -> Result<Vec<u8>, StorageError> {
    let length = encoded
        .get(*offset..offset.saturating_add(8))
        .ok_or_else(|| StorageError::invalid("receipt key field is truncated"))?;
    let length = u64::from_be_bytes(
        length
            .try_into()
            .map_err(|_| StorageError::invalid("receipt key length is invalid"))?,
    );
    *offset = offset
        .checked_add(8)
        .ok_or_else(|| StorageError::invalid("receipt key offset overflow"))?;
    let length = usize::try_from(length)
        .map_err(|_| StorageError::invalid("receipt key field is too large"))?;
    let end = offset
        .checked_add(length)
        .ok_or_else(|| StorageError::invalid("receipt key field overflows"))?;
    let field = encoded
        .get(*offset..end)
        .ok_or_else(|| StorageError::invalid("receipt key field is truncated"))?
        .to_vec();
    *offset = end;
    Ok(field)
}

/// One opaque, canonical `AuditEvent` waiting for the Control Plane audit
/// store. Storage owns only the atomic receipt bridge; the audit crate owns
/// the payload schema and hash chain.
///
/// Storage owns the atomicity and idempotency boundary, while the Control
/// Plane owns the payload's audit schema. Keeping the payload opaque avoids a
/// dependency from storage back into the audit or application crates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingAuditEvent {
    event_id: String,
    payload: Vec<u8>,
}

impl PendingAuditEvent {
    /// Builds one opaque pending event for the same scoped receipt transaction.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::InvalidInput`] for an empty event identity
    /// or empty encoded event.
    pub fn new(
        event_id: impl Into<String>,
        payload: impl Into<Vec<u8>>,
    ) -> Result<Self, StorageError> {
        let event = Self {
            event_id: event_id.into(),
            payload: payload.into(),
        };
        event.validate()?;
        Ok(event)
    }

    #[must_use]
    pub fn event_id(&self) -> &str {
        &self.event_id
    }

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    fn validate(&self) -> Result<(), StorageError> {
        if self.event_id.is_empty() {
            return Err(StorageError::invalid(
                "pending audit event_id must not be empty",
            ));
        }
        if self.payload.is_empty() {
            return Err(StorageError::invalid(
                "pending audit event payload must not be empty",
            ));
        }
        Ok(())
    }
}

/// One secondary canonical-state revision that must still be current when a
/// [`StateCommit`] is written.
///
/// A guard is evaluated in the same `SQLite` `IMMEDIATE` transaction as the
/// guarded commit. A stream that has not been persisted yet has revision zero.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateRevisionGuard {
    stream_id: String,
    expected_revision: u64,
}

impl StateRevisionGuard {
    /// Creates one exact secondary-stream revision guard.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::InvalidInput`] when the stream identity is
    /// empty or the revision cannot be represented by `SQLite`.
    pub fn new(stream_id: impl Into<String>, expected_revision: u64) -> Result<Self, StorageError> {
        let stream_id = stream_id.into();
        if stream_id.is_empty() {
            return Err(StorageError::invalid(
                "state revision guard stream_id must not be empty",
            ));
        }
        if expected_revision > i64::MAX as u64 {
            return Err(StorageError::invalid(
                "state revision guard expected_revision exceeds the SQLite integer range",
            ));
        }
        Ok(Self {
            stream_id,
            expected_revision,
        })
    }

    #[must_use]
    pub fn stream_id(&self) -> &str {
        &self.stream_id
    }

    #[must_use]
    pub const fn expected_revision(&self) -> u64 {
        self.expected_revision
    }
}

/// One secondary canonical-state write staged inside a [`StateCommit`].
///
/// The mutation uses compare-and-swap semantics: its stream must still have
/// `expected_revision` when the receipt transaction starts writing. Secondary
/// mutations do not own a receipt, journal publication, audit event, or
/// outbox event of their own.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateMutation {
    stream_id: String,
    expected_revision: u64,
    state: Vec<u8>,
}

impl StateMutation {
    /// Creates one typed secondary canonical-state mutation.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::InvalidInput`] when the stream identity is
    /// empty or its next revision cannot be represented by `SQLite`.
    pub fn new(
        stream_id: impl Into<String>,
        expected_revision: u64,
        state: impl Into<Vec<u8>>,
    ) -> Result<Self, StorageError> {
        let mutation = Self {
            stream_id: stream_id.into(),
            expected_revision,
            state: state.into(),
        };
        mutation.validate()?;
        Ok(mutation)
    }

    #[must_use]
    pub fn stream_id(&self) -> &str {
        &self.stream_id
    }

    #[must_use]
    pub const fn expected_revision(&self) -> u64 {
        self.expected_revision
    }

    #[must_use]
    pub fn state(&self) -> &[u8] {
        &self.state
    }

    fn validate(&self) -> Result<(), StorageError> {
        if self.stream_id.is_empty() {
            return Err(StorageError::invalid(
                "state mutation stream_id must not be empty",
            ));
        }
        if self.expected_revision >= i64::MAX as u64 {
            return Err(StorageError::invalid(
                "state mutation next revision exceeds the SQLite integer range",
            ));
        }
        Ok(())
    }
}

/// Maximum secondary canonical-state writes accepted by one receipt.
pub const MAX_STATE_MUTATIONS_PER_COMMIT: usize = 16;
/// Maximum bytes accepted from one secondary canonical-state payload.
pub const MAX_STATE_MUTATION_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
/// Maximum combined secondary canonical-state payload bytes in one receipt.
pub const MAX_STATE_MUTATION_BYTES_PER_COMMIT: usize = 16 * 1024 * 1024;

/// One atomic canonical-state and outbox commit at the storage port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateCommit {
    pub receipt_identity: ReceiptIdentity,
    pub command_digest: Sha256Digest,
    pub stream_id: String,
    pub expected_revision: u64,
    pub state: Vec<u8>,
    pub events: Vec<NewOutboxEvent>,
    state_guards: Vec<StateRevisionGuard>,
    state_mutations: Vec<StateMutation>,
    journal_publication: Option<AggregateJournalPublication>,
    pending_audit_event: Option<PendingAuditEvent>,
    receipt_replay_required: bool,
}

impl StateCommit {
    #[must_use]
    pub fn new(
        receipt_identity: ReceiptIdentity,
        command_digest: Sha256Digest,
        stream_id: impl Into<String>,
        expected_revision: u64,
        state: impl Into<Vec<u8>>,
        events: Vec<NewOutboxEvent>,
    ) -> Self {
        Self {
            receipt_identity,
            command_digest,
            stream_id: stream_id.into(),
            expected_revision,
            state: state.into(),
            events,
            state_guards: Vec::new(),
            state_mutations: Vec::new(),
            journal_publication: None,
            pending_audit_event: None,
            receipt_replay_required: false,
        }
    }

    /// Adds one opaque aggregate publication to the same state transaction.
    #[must_use]
    pub fn with_journal_publication(mut self, publication: AggregateJournalPublication) -> Self {
        self.journal_publication = Some(publication);
        self
    }

    #[must_use]
    pub const fn journal_publication(&self) -> Option<&AggregateJournalPublication> {
        self.journal_publication.as_ref()
    }

    /// Adds one canonical audit event to this same state/receipt/outbox
    /// transaction.
    #[must_use]
    pub fn with_pending_audit_event(mut self, pending_audit_event: PendingAuditEvent) -> Self {
        self.pending_audit_event = Some(pending_audit_event);
        self
    }

    #[must_use]
    pub const fn pending_audit_event(&self) -> Option<&PendingAuditEvent> {
        self.pending_audit_event.as_ref()
    }

    /// Adds one secondary canonical-state revision guard to this commit.
    #[must_use]
    pub fn with_state_guard(mut self, guard: StateRevisionGuard) -> Self {
        self.state_guards.push(guard);
        self
    }

    #[must_use]
    pub fn state_guards(&self) -> &[StateRevisionGuard] {
        &self.state_guards
    }

    /// Adds one secondary canonical-state compare-and-swap write.
    #[must_use]
    pub fn with_state_mutation(mut self, mutation: StateMutation) -> Self {
        self.state_mutations.push(mutation);
        self
    }

    #[must_use]
    pub fn state_mutations(&self) -> &[StateMutation] {
        &self.state_mutations
    }

    /// Returns whether this commit may only replay an already durable receipt.
    #[must_use]
    pub const fn receipt_replay_required(&self) -> bool {
        self.receipt_replay_required
    }

    /// Runs the canonical validation shared by every storage adapter.
    ///
    /// # Errors
    ///
    /// Returns the adapter-neutral validation failure before any transaction
    /// or database-specific operation begins.
    pub fn validate_for_storage_adapter(&self) -> Result<(), StorageError> {
        self.validate()
    }

    /// Requires this call to resolve an already durable scoped receipt.
    ///
    /// Aggregate adapters use this after their journal reports the request as
    /// a replay. If an older unsafe composition left only the journal record,
    /// storage fails closed instead of persisting recomputed state or events.
    #[must_use]
    pub fn require_receipt_replay(mut self) -> Self {
        self.receipt_replay_required = true;
        self
    }

    fn validate(&self) -> Result<(), StorageError> {
        validate_sha256_digest(&self.command_digest)?;
        if self.stream_id.is_empty() {
            return Err(StorageError::invalid("stream_id must not be empty"));
        }
        if self.events.is_empty() {
            return Err(StorageError::invalid(
                "a state commit must contain at least one outbox event",
            ));
        }
        if self.expected_revision > i64::MAX as u64 {
            return Err(StorageError::invalid(
                "expected_revision exceeds the SQLite integer range",
            ));
        }
        self.validate_secondary_states()?;
        if let Some(publication) = &self.journal_publication {
            publication.validate()?;
        }
        if let Some(pending_audit_event) = &self.pending_audit_event {
            pending_audit_event.validate()?;
        }
        self.validate_events()
    }

    fn validate_secondary_states(&self) -> Result<(), StorageError> {
        let mut guard_streams = HashSet::with_capacity(self.state_guards.len());
        for guard in &self.state_guards {
            if guard.stream_id.is_empty() {
                return Err(StorageError::invalid(
                    "state revision guard stream_id must not be empty",
                ));
            }
            if guard.expected_revision > i64::MAX as u64 {
                return Err(StorageError::invalid(
                    "state revision guard expected_revision exceeds the SQLite integer range",
                ));
            }
            if guard.stream_id == self.stream_id {
                return Err(StorageError::invalid(
                    "state revision guard must not target the primary stream",
                ));
            }
            if !guard_streams.insert(guard.stream_id.as_str()) {
                return Err(StorageError::invalid(
                    "state revision guard stream ids must be unique",
                ));
            }
        }
        if self.state_mutations.len() > MAX_STATE_MUTATIONS_PER_COMMIT {
            return Err(StorageError::invalid(format!(
                "a state commit must contain at most {MAX_STATE_MUTATIONS_PER_COMMIT} secondary mutations"
            )));
        }
        let mut mutation_bytes = 0_usize;
        let mut mutation_streams = HashSet::with_capacity(self.state_mutations.len());
        for mutation in &self.state_mutations {
            mutation.validate()?;
            if mutation.stream_id == self.stream_id {
                return Err(StorageError::invalid(
                    "state mutation must not target the primary stream",
                ));
            }
            if guard_streams.contains(mutation.stream_id.as_str()) {
                return Err(StorageError::invalid(
                    "state mutation must not target a guarded stream",
                ));
            }
            if !mutation_streams.insert(mutation.stream_id.as_str()) {
                return Err(StorageError::invalid(
                    "state mutation stream ids must be unique",
                ));
            }
            if mutation.state.len() > MAX_STATE_MUTATION_PAYLOAD_BYTES {
                return Err(StorageError::invalid(format!(
                    "a state mutation payload must contain at most {MAX_STATE_MUTATION_PAYLOAD_BYTES} bytes"
                )));
            }
            mutation_bytes = mutation_bytes
                .checked_add(mutation.state.len())
                .ok_or_else(|| StorageError::invalid("state mutation payload size overflow"))?;
        }
        if mutation_bytes > MAX_STATE_MUTATION_BYTES_PER_COMMIT {
            return Err(StorageError::invalid(format!(
                "secondary state mutation payloads must total at most {MAX_STATE_MUTATION_BYTES_PER_COMMIT} bytes"
            )));
        }
        Ok(())
    }

    fn validate_events(&self) -> Result<(), StorageError> {
        let mut event_ids = HashSet::with_capacity(self.events.len());
        for event in &self.events {
            if event.event_id.is_empty() {
                return Err(StorageError::invalid("event_id must not be empty"));
            }
            if event.topic.is_empty() {
                return Err(StorageError::invalid("event topic must not be empty"));
            }
            if !event_ids.insert(event.event_id.as_str()) {
                return Err(StorageError::invalid(
                    "event ids must be unique inside one commit",
                ));
            }
            if let Some(stream) = event.projection_stream() {
                stream.validate()?;
                if !canonical_control_plane_event_id(&event.event_id) {
                    return Err(StorageError::invalid(
                        "projection event_id must be a canonical ControlPlaneEventId",
                    ));
                }
                let context = event.public_context().ok_or_else(|| {
                    StorageError::invalid("public projection event context is required")
                })?;
                if stream != context.stream() {
                    return Err(StorageError::invalid(
                        "public event context stream differs from its projection stream",
                    ));
                }
                validate_public_event_time(context.occurred_at())?;
                validate_public_source(context.source())?;
                validate_public_stream_source(stream, context.source())?;
                if &receipt_scope_key(context.scope())? != self.receipt_identity.scope_key() {
                    return Err(StorageError::invalid(
                        "public event scope differs from its receipt scope",
                    ));
                }
                if let PublicEventSource::ControlPlane { actor, .. } = context.source()
                    && &receipt_actor_key(actor)? != self.receipt_identity.actor_key()
                {
                    return Err(StorageError::invalid(
                        "public Control Plane source actor differs from its receipt actor",
                    ));
                }
            } else if event.public_context().is_some() {
                return Err(StorageError::invalid(
                    "internal outbox event must not carry public context",
                ));
            }
        }
        Ok(())
    }
}

/// Canonical state loaded through the storage seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredState {
    pub stream_id: String,
    pub revision: u64,
    pub payload: Vec<u8>,
}

/// One state-directory row sealed without retaining its potentially large
/// payload in the caller's directory snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredStateDirectoryEntry {
    pub stream_id: String,
    pub revision: u64,
    pub payload_sha256: Sha256Digest,
}

/// One consistent read of canonical state streams and a resource-local public
/// event cursor.
///
/// A `SQLite` implementation obtains every value in this structure through one
/// read transaction. The Control Plane uses it to prevent a runtime snapshot
/// from being paired with a cursor observed after a concurrent commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionReadCut {
    states: Vec<StoredState>,
    projection_event_cursor: ProjectionEventCursor,
}

impl ProjectionReadCut {
    #[must_use]
    pub fn states(&self) -> &[StoredState] {
        &self.states
    }

    #[must_use]
    pub const fn projection_event_cursor(&self) -> &ProjectionEventCursor {
        &self.projection_event_cursor
    }

    pub(crate) fn new(
        states: Vec<StoredState>,
        projection_event_cursor: ProjectionEventCursor,
    ) -> Self {
        Self {
            states,
            projection_event_cursor,
        }
    }
}

/// A durable event waiting to be published.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboxEvent {
    pub sequence: u64,
    pub event_id: String,
    pub topic: String,
    pub payload: Vec<u8>,
    /// Present only for a secret-safe resource-stream event.
    pub projection_cursor: Option<ProjectionEventCursor>,
    /// Complete immutable envelope facts, present exactly when this is public.
    pub public_context: Option<PublicProjectionEventContext>,
}

/// One exact durable outbox row together with the receipt that authorized it.
///
/// Unlike [`ProductStateStorage::pending_events`], this read is independent of
/// publication acknowledgement. Application transactions use it to join a
/// Worker message to the immutable `ExecutionJob` that was committed before
/// dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableOutboxEvent {
    event: OutboxEvent,
    receipt_identity: ReceiptIdentity,
    command_digest: Sha256Digest,
    stream_id: String,
    revision: u64,
}

impl DurableOutboxEvent {
    #[must_use]
    pub const fn event(&self) -> &OutboxEvent {
        &self.event
    }

    #[must_use]
    pub const fn receipt_identity(&self) -> &ReceiptIdentity {
        &self.receipt_identity
    }

    #[must_use]
    pub const fn command_digest(&self) -> &Sha256Digest {
        &self.command_digest
    }

    #[must_use]
    pub fn stream_id(&self) -> &str {
        &self.stream_id
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }
}

/// The durable result of one state commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitReceipt {
    pub receipt_identity: ReceiptIdentity,
    /// Exact canonical digest accepted for this scoped request.
    pub command_digest: Sha256Digest,
    pub stream_id: String,
    pub revision: u64,
    /// Exact durable events attached to the original scoped request.
    ///
    /// Replays return these stored bytes rather than values recomputed by a
    /// retry, so application adapters can recover the original dispatch job.
    pub events: Vec<OutboxEvent>,
    pub idempotent_replay: bool,
}

/// One atomic receipt proving a canonical state mutation and its schedulable
/// execution job were committed or replayed together.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateExecutionJobCommitReceipt {
    pub state: CommitReceipt,
    pub execution_job: ExecutionJobMutationReceipt,
}

/// Stable error categories exposed by storage adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageErrorKind {
    InvalidInput,
    RevisionConflict,
    RequestConflict,
    RequestReplayMissing,
    JournalAlreadyExists,
    JournalNotFound,
    JournalConflict,
    EventCursorExpired,
    Adapter,
    Closed,
}

/// Storage failure with an adapter-neutral category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageError {
    kind: StorageErrorKind,
    message: String,
    state_guard_conflict: bool,
}

impl StorageError {
    #[must_use]
    pub fn adapter(message: impl Into<String>) -> Self {
        Self {
            kind: StorageErrorKind::Adapter,
            message: message.into(),
            state_guard_conflict: false,
        }
    }

    #[must_use]
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self {
            kind: StorageErrorKind::InvalidInput,
            message: message.into(),
            state_guard_conflict: false,
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::invalid_input(message)
    }

    fn closed() -> Self {
        Self {
            kind: StorageErrorKind::Closed,
            message: "storage is already closed".to_owned(),
            state_guard_conflict: false,
        }
    }

    /// Builds the adapter-neutral concurrency result used by application
    /// adapters that already validated a typed aggregate revision conflict.
    #[must_use]
    pub fn revision_conflict(expected: u64, actual: u64) -> Self {
        Self {
            kind: StorageErrorKind::RevisionConflict,
            message: format!("expected revision {expected}, but current revision is {actual}"),
            state_guard_conflict: false,
        }
    }

    /// Builds a revision conflict for a secondary [`StateRevisionGuard`].
    #[must_use]
    pub fn state_revision_guard_conflict(stream_id: &str, expected: u64, actual: u64) -> Self {
        Self {
            kind: StorageErrorKind::RevisionConflict,
            message: format!(
                "state revision guard {stream_id} expected revision {expected}, but current revision is {actual}"
            ),
            state_guard_conflict: true,
        }
    }

    /// Builds a compare-and-swap conflict for a secondary [`StateMutation`].
    #[must_use]
    pub fn state_mutation_revision_conflict(stream_id: &str, expected: u64, actual: u64) -> Self {
        Self {
            kind: StorageErrorKind::RevisionConflict,
            message: format!(
                "state mutation {stream_id} expected revision {expected}, but current revision is {actual}"
            ),
            state_guard_conflict: false,
        }
    }

    /// Builds the revision-token conflict for the one reviewed solution-set
    /// digest accepted at the task-promotion boundary.
    #[must_use]
    pub fn revision_token_conflict(field: &str) -> Self {
        if field != "reviewSetSha256" {
            return Self::invalid_input("revision token conflict field is unsupported");
        }
        Self {
            kind: StorageErrorKind::RevisionConflict,
            message: "reviewSetSha256 no longer identifies the current solution review".to_owned(),
            state_guard_conflict: false,
        }
    }

    /// Builds the adapter-neutral result for a reused scoped request whose
    /// canonical command digest differs from the first committed command.
    #[must_use]
    pub fn request_conflict(request_id: &RequestId) -> Self {
        Self {
            kind: StorageErrorKind::RequestConflict,
            message: format!(
                "request id {} was already used for another command in this actor and scope",
                request_id.0
            ),
            state_guard_conflict: false,
        }
    }

    fn request_replay_missing(request_id: &RequestId) -> Self {
        Self {
            kind: StorageErrorKind::RequestReplayMissing,
            message: format!(
                "request id {} exists in the aggregate journal without its scoped command receipt",
                request_id.0
            ),
            state_guard_conflict: false,
        }
    }

    fn journal_already_exists(key: &AggregateJournalKey) -> Self {
        Self {
            kind: StorageErrorKind::JournalAlreadyExists,
            message: format!(
                "{} aggregate journal {} already exists",
                key.aggregate_type, key.aggregate_id
            ),
            state_guard_conflict: false,
        }
    }

    fn journal_not_found(key: &AggregateJournalKey) -> Self {
        Self {
            kind: StorageErrorKind::JournalNotFound,
            message: format!(
                "{} aggregate journal {} does not exist",
                key.aggregate_type, key.aggregate_id
            ),
            state_guard_conflict: false,
        }
    }

    fn journal_conflict(key: &AggregateJournalKey) -> Self {
        Self {
            kind: StorageErrorKind::JournalConflict,
            message: format!(
                "{} aggregate journal {} tail changed",
                key.aggregate_type, key.aggregate_id
            ),
            state_guard_conflict: false,
        }
    }

    fn event_cursor_expired() -> Self {
        Self {
            kind: StorageErrorKind::EventCursorExpired,
            message: "projection event cursor is outside the retained stream window".to_owned(),
            state_guard_conflict: false,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> StorageErrorKind {
        self.kind
    }

    /// Returns whether this revision conflict came from a secondary
    /// [`StateRevisionGuard`] rather than the primary commit stream.
    #[must_use]
    pub const fn is_state_guard_conflict(&self) -> bool {
        self.state_guard_conflict
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StorageError {}

/// Deep storage seam shared by the `SQLite` adapter and a future `PostgreSQL` adapter.
///
/// `commit` owns the full transaction. `pending_events` and `mark_published`
/// implement an at-least-once outbox with stable event ids.
pub trait ProductStateStorage: Send {
    /// Atomically writes canonical state, its request receipt, and outbox
    /// events. Every [`StateRevisionGuard`] and [`StateMutation`] on `commit`
    /// is checked after the receipt lookup and before the first write in that
    /// same transaction. Secondary mutations are then written in that same
    /// transaction without creating separate receipts or journal records.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when validation, concurrency control,
    /// request idempotency, or the adapter transaction fails.
    fn commit(&mut self, commit: &StateCommit) -> Result<CommitReceipt, StorageError> {
        commit.validate()?;
        if !commit.state_mutations().is_empty() {
            return Err(StorageError::adapter(
                "secondary state mutations require an atomic storage adapter",
            ));
        }
        self.commit_adapter(commit)
    }

    /// Atomically writes canonical product state and exactly one scheduler
    /// execution job under the same database transaction.
    ///
    /// An exact request retry requires both original receipts. A partial
    /// historical write is rejected instead of creating the missing half on
    /// replay.
    ///
    /// # Errors
    ///
    /// Returns an adapter error unless the storage implementation can prove
    /// the state receipt, queue receipt, job row, and outbox share one commit.
    fn commit_with_execution_job(
        &mut self,
        commit: &StateCommit,
        submission: &ExecutionJobSubmission,
    ) -> Result<StateExecutionJobCommitReceipt, StorageError> {
        commit.validate()?;
        validate_state_execution_job_authority(commit, submission)?;
        Err(StorageError::adapter(
            "atomic state and execution job commit is unavailable",
        ))
    }

    /// Loads the canonical scheduler row for one globally unique execution Job.
    ///
    /// Local production storage overrides this seam so every Control Plane
    /// transaction resolves an attempt replacement before validating a Worker
    /// frame. Non-scheduler adapters return no row.
    ///
    /// # Errors
    ///
    /// Returns storage corruption or adapter failures.
    fn load_execution_job_record(
        &self,
        _job_id: &ExecutionJobId,
    ) -> Result<Option<ExecutionJobRecord>, StorageError> {
        Ok(None)
    }

    /// Loads the scheduler-sealed predecessor-to-successor authority for a Job.
    ///
    /// # Errors
    ///
    /// Returns storage corruption or adapter failures.
    fn load_execution_scope_replacement_authority(
        &self,
        _job_id: &ExecutionJobId,
    ) -> Result<Option<ExecutionScopeReplacementAuthority>, StorageError> {
        Ok(None)
    }

    /// Adapter implementation hook behind the default [`Self::commit`].
    ///
    /// The default boundary invokes this hook only after validation and only
    /// when the commit has no secondary mutations. An adapter that proves the
    /// full atomic mutation contract overrides [`Self::commit`] instead.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the adapter transaction fails or
    /// the adapter is read-only.
    #[doc(hidden)]
    fn commit_adapter(&mut self, _commit: &StateCommit) -> Result<CommitReceipt, StorageError> {
        Err(StorageError::adapter("state commit storage is unavailable"))
    }

    /// Loads the original durable result for one scoped command identity.
    ///
    /// A matching receipt is returned as an idempotent replay. Reusing the
    /// same actor, scope, and request id with another command digest fails
    /// with [`StorageErrorKind::RequestConflict`].
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the digest is malformed, the
    /// scoped request conflicts, or the read fails.
    fn load_receipt(
        &self,
        identity: &ReceiptIdentity,
        command_digest: &Sha256Digest,
    ) -> Result<Option<CommitReceipt>, StorageError>;

    /// Loads the durable result for an exact scoped request without comparing
    /// a supplied command body. This is used only to report the committed
    /// cursor after a changed-body idempotency conflict; it does not authorize
    /// replay or reinterpret any current aggregate state.
    ///
    /// Adapters that do not provide this lookup fail closed with an adapter
    /// error, and callers must return a zero acknowledgement cursor.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the lookup is unavailable or the
    /// storage adapter cannot read the receipt.
    fn load_receipt_for_identity(
        &self,
        _identity: &ReceiptIdentity,
    ) -> Result<Option<CommitReceipt>, StorageError> {
        Err(StorageError::adapter(
            "receipt lookup without a command digest is unavailable",
        ))
    }

    /// Loads the one durable command receipt that advanced an exact state
    /// stream revision.
    ///
    /// This lookup is used by recovery compositions that observe a terminal
    /// aggregate after the original command response has been lost.  It never
    /// reconstructs a receipt from the state payload: the actor, scope,
    /// request, digest, and outbox events all come from the canonical receipt
    /// row.  Adapters that cannot provide this exact lookup fail closed.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the stream/revision is invalid,
    /// the receipt directory is ambiguous, or the adapter cannot read it.
    fn load_receipt_for_stream_revision(
        &self,
        _stream_id: &str,
        _revision: u64,
    ) -> Result<Option<CommitReceipt>, StorageError> {
        Err(StorageError::adapter(
            "receipt lookup by stream revision is unavailable",
        ))
    }

    /// Loads the pending canonical audit event attached to one scoped receipt,
    /// whether or not the Control Plane has flushed it to `AuditStore`.
    ///
    /// Adapters that do not yet persist pending audit events fail closed rather
    /// than manufacturing a value from a receipt or outbox event.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when pending audit-event lookup is
    /// unavailable or the storage adapter cannot read the event.
    fn load_pending_audit_event(
        &self,
        _identity: &ReceiptIdentity,
    ) -> Result<Option<PendingAuditEvent>, StorageError> {
        Err(StorageError::adapter(
            "pending audit event storage is unavailable",
        ))
    }

    /// Loads canonical audit events that have committed with state but have
    /// not yet been appended to the immutable `AuditStore`.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the storage adapter cannot read
    /// the pending audit-event outbox.
    fn pending_audit_events(&self) -> Result<Vec<PendingAuditEvent>, StorageError> {
        Ok(Vec::new())
    }

    /// Marks one pending canonical audit event as durably appended to the
    /// immutable `AuditStore`. The marker is idempotent so a crash between
    /// append and marker update is recovered by retrying the same event id.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the event id is invalid, the
    /// event is missing, or the storage adapter cannot update the marker.
    fn mark_audit_event_persisted(&mut self, _event_id: &str) -> Result<(), StorageError> {
        Err(StorageError::adapter(
            "pending audit event acknowledgement is unavailable",
        ))
    }

    /// Loads one exact durable event by stable event id, whether or not it was
    /// already acknowledged as published.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral read failure. Adapters that have not yet
    /// implemented exact durable event lookup fail closed.
    fn load_outbox_event(
        &self,
        _event_id: &str,
    ) -> Result<Option<DurableOutboxEvent>, StorageError> {
        Err(StorageError::adapter(
            "exact durable outbox event lookup is unavailable",
        ))
    }

    /// Loads the current canonical state for one stream.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or storage is closed.
    fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError>;

    /// Returns the greatest current stream identity under one exact prefix.
    ///
    /// This is the immutable upper bound used by stable keyset walks. Adapters
    /// that do not expose bounded state enumeration fail closed.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the prefix is invalid, the read
    /// is unavailable, or storage is closed.
    fn last_state_stream_id(&self, _prefix: &str) -> Result<Option<String>, StorageError> {
        Err(StorageError::adapter(
            "bounded state stream enumeration is unavailable",
        ))
    }

    /// Scans one bounded keyset page under an exact prefix. The returned rows
    /// are ordered by stream identity and never exceed `limit` or
    /// `upper_bound`.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the bounds are invalid, the read
    /// is unavailable, or storage is closed.
    fn scan_state_streams(
        &self,
        _prefix: &str,
        _after: &str,
        _upper_bound: &str,
        _limit: usize,
    ) -> Result<Vec<StoredState>, StorageError> {
        Err(StorageError::adapter(
            "bounded state stream enumeration is unavailable",
        ))
    }

    /// Loads one complete, bounded state-stream directory from a single
    /// adapter read cut. Rows are ordered by stream identity. Unlike paged
    /// enumeration, this seam cannot omit a lower-sorting row inserted after
    /// an earlier page has already been consumed.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the prefix or bound is invalid,
    /// the directory exceeds `max_entries` or `max_payload_bytes`, the atomic
    /// read is unavailable, or storage is closed.
    fn load_bounded_state_directory(
        &self,
        _prefix: &str,
        _max_entries: usize,
        _max_payload_bytes: usize,
    ) -> Result<Vec<StoredStateDirectoryEntry>, StorageError> {
        Err(StorageError::adapter(
            "atomic bounded state directory reads are unavailable",
        ))
    }

    /// Loads one bounded, atomic directory cut containing only streams at the
    /// exact current revision. This lets restart reconcilers bound unresolved
    /// work without retaining every historical terminal stream in the scan.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the revision or bounds are
    /// invalid, the matching directory exceeds a bound, the atomic read is
    /// unavailable, or storage is closed.
    fn load_bounded_state_directory_at_revision(
        &self,
        _prefix: &str,
        _revision: u64,
        _max_entries: usize,
        _max_payload_bytes: usize,
    ) -> Result<Vec<StoredStateDirectoryEntry>, StorageError> {
        Err(StorageError::adapter(
            "atomic revision-filtered state directory reads are unavailable",
        ))
    }

    /// Loads canonical state streams and the resource-local public event
    /// cursor from one durable read cut.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when either state or cursor loading
    /// fails.
    fn load_projection_read_cut(
        &self,
        state_stream_ids: &[String],
        key: &ProjectionEventStreamKey,
        expected: Option<&ProjectionEventCursor>,
    ) -> Result<ProjectionReadCut, StorageError>;

    /// Loads one fully committed opaque aggregate journal.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or storage is closed.
    fn load_journal(
        &self,
        key: &AggregateJournalKey,
    ) -> Result<Option<LoadedAggregateJournal>, StorageError>;

    /// Loads the latest retained position, or validates one exact historical
    /// position, in a tenant-scoped resource event stream.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::EventCursorExpired`] only when a previously
    /// persisted positive position is no longer retained.
    fn load_projection_event_cursor(
        &self,
        _key: &ProjectionEventStreamKey,
        _expected: Option<&ProjectionEventCursor>,
    ) -> Result<ProjectionEventCursor, StorageError> {
        Err(StorageError::adapter(
            "projection event stream storage is unavailable",
        ))
    }

    /// Loads all unpublished events in durable sequence order.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or storage is closed.
    fn pending_events(&self) -> Result<Vec<OutboxEvent>, StorageError>;

    /// Marks one stable event id as published.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the event is missing, the write
    /// fails, or storage is closed.
    fn mark_published(&mut self, event_id: &str) -> Result<(), StorageError>;

    /// Deterministically closes the adapter and releases its resources.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when checkpointing or close fails.
    fn close(self: Box<Self>) -> Result<(), StorageError>;
}

/// Local `SQLite` implementation of [`ProductStateStorage`].
pub struct SqliteStorage {
    connection: Option<Connection>,
    read_connection: Mutex<Connection>,
    database_path: PathBuf,
}

impl SqliteStorage {
    /// Opens the local database and applies all schema migrations before return.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the directory, connection,
    /// durability settings, or schema migration cannot be prepared.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, StorageError> {
        let open_deadline = StdInstant::now() + SQLITE_OPEN_TIMEOUT;
        let data_directory = data_directory.as_ref();
        fs::create_dir_all(data_directory).map_err(|error| {
            StorageError::adapter(format!("failed to create the data directory: {error}"))
        })?;
        let canonical_data_directory = fs::canonicalize(data_directory).map_err(|error| {
            StorageError::adapter(format!("failed to resolve the data directory: {error}"))
        })?;
        let database_path = canonical_data_directory.join(DATABASE_FILE_NAME);
        // SQLite schema and journal-mode setup acquire database-wide locks. Serialize only
        // callers opening the same canonical database; unrelated repositories must not consume
        // one another's bounded initialization window.
        let open_lock = sqlite_open_lock(&database_path, open_deadline)?;
        let _open_guard = acquire_mutex_before_open_deadline(&open_lock, open_deadline)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX;
        let mut connection =
            Connection::open_with_flags(&database_path, flags).map_err(sql_error)?;
        set_open_busy_deadline(&connection, open_deadline)?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(sql_error)?;
        set_open_busy_deadline(&connection, open_deadline)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_error)?;
        set_open_busy_deadline(&connection, open_deadline)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sql_error)?;
        set_open_busy_deadline(&connection, open_deadline)?;
        apply_migrations(&mut connection)?;

        let read_connection =
            Connection::open_with_flags(&database_path, flags).map_err(sql_error)?;
        set_open_busy_deadline(&read_connection, open_deadline)?;
        read_connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(sql_error)?;
        set_open_busy_deadline(&read_connection, open_deadline)?;
        read_connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sql_error)?;
        connection
            .busy_timeout(SQLITE_BUSY_TIMEOUT)
            .map_err(sql_error)?;
        read_connection
            .busy_timeout(SQLITE_BUSY_TIMEOUT)
            .map_err(sql_error)?;

        Ok(Self {
            connection: Some(connection),
            read_connection: Mutex::new(read_connection),
            database_path,
        })
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub(crate) fn connection(&self) -> Result<&Connection, StorageError> {
        self.connection.as_ref().ok_or_else(StorageError::closed)
    }

    pub(crate) fn connection_mut(&mut self) -> Result<&mut Connection, StorageError> {
        self.connection.as_mut().ok_or_else(StorageError::closed)
    }
}

fn sqlite_open_lock(
    database_path: &Path,
    open_deadline: StdInstant,
) -> Result<Arc<Mutex<()>>, StorageError> {
    let locks = SQLITE_OPEN_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = acquire_mutex_before_open_deadline(locks, open_deadline)?;
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(database_path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(database_path.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}

fn acquire_mutex_before_open_deadline<T>(
    mutex: &Mutex<T>,
    open_deadline: StdInstant,
) -> Result<MutexGuard<'_, T>, StorageError> {
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(TryLockError::WouldBlock) => {
                let remaining = remaining_sqlite_open_time(open_deadline)?;
                thread::sleep(remaining.min(SQLITE_OPEN_RETRY_INTERVAL));
            }
        }
    }
}

fn set_open_busy_deadline(
    connection: &Connection,
    open_deadline: StdInstant,
) -> Result<(), StorageError> {
    remaining_sqlite_open_time(open_deadline)?;
    SQLITE_OPEN_BUSY_DEADLINE.set(Some(open_deadline));
    connection
        .busy_handler(Some(sqlite_open_busy_handler))
        .map_err(sql_error)
}

fn sqlite_open_busy_handler(_prior_calls: i32) -> bool {
    SQLITE_OPEN_BUSY_DEADLINE.get().is_some_and(|deadline| {
        let Some(remaining) = deadline.checked_duration_since(StdInstant::now()) else {
            return false;
        };
        thread::sleep(remaining.min(SQLITE_OPEN_RETRY_INTERVAL));
        true
    })
}

fn remaining_sqlite_open_time(open_deadline: StdInstant) -> Result<Duration, StorageError> {
    open_deadline
        .checked_duration_since(StdInstant::now())
        .filter(|remaining| *remaining >= Duration::from_millis(1))
        .ok_or_else(|| StorageError::adapter("SQLite storage open exceeded its five-second limit"))
}

impl ProductStateStorage for SqliteStorage {
    fn commit(&mut self, commit: &StateCommit) -> Result<CommitReceipt, StorageError> {
        commit.validate()?;
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let receipt = commit_in_transaction(&transaction, commit)?;
        transaction.commit().map_err(sql_error)?;
        Ok(receipt)
    }

    fn commit_with_execution_job(
        &mut self,
        commit: &StateCommit,
        submission: &ExecutionJobSubmission,
    ) -> Result<StateExecutionJobCommitReceipt, StorageError> {
        commit.validate()?;
        validate_state_execution_job_authority(commit, submission)?;
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let state = commit_in_transaction(&transaction, commit)?;
        let mode = if state.idempotent_replay {
            execution_queue::ExecutionJobSubmissionMode::RequireReplay
        } else {
            execution_queue::ExecutionJobSubmissionMode::RequireNew
        };
        let execution_job =
            execution_queue::submit_execution_job_in_transaction(&transaction, submission, mode)?;
        transaction.commit().map_err(sql_error)?;
        Ok(StateExecutionJobCommitReceipt {
            state,
            execution_job,
        })
    }

    fn load_execution_job_record(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<ExecutionJobRecord>, StorageError> {
        repository_scheduler::load_execution_job_by_id(self.connection()?, job_id)
    }

    fn load_execution_scope_replacement_authority(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<ExecutionScopeReplacementAuthority>, StorageError> {
        execution_scope_replacement::load_execution_scope_replacement(self.connection()?, job_id)
    }

    fn load_receipt(
        &self,
        identity: &ReceiptIdentity,
        command_digest: &Sha256Digest,
    ) -> Result<Option<CommitReceipt>, StorageError> {
        validate_sha256_digest(command_digest)?;
        let connection = self.connection()?;
        let Some(prior) = prior_receipt(connection, identity)? else {
            return Ok(None);
        };
        replay_stored_receipt(connection, identity, command_digest, prior).map(Some)
    }

    fn load_receipt_for_identity(
        &self,
        identity: &ReceiptIdentity,
    ) -> Result<Option<CommitReceipt>, StorageError> {
        let connection = self.connection()?;
        let Some(prior) = prior_receipt(connection, identity)? else {
            return Ok(None);
        };
        let command_digest = Sha256Digest(prior.command_digest.clone());
        replay_stored_receipt(connection, identity, &command_digest, prior).map(Some)
    }

    fn load_receipt_for_stream_revision(
        &self,
        stream_id: &str,
        revision: u64,
    ) -> Result<Option<CommitReceipt>, StorageError> {
        if stream_id.is_empty() || revision == 0 {
            return Err(StorageError::invalid(
                "receipt stream revision lookup is invalid",
            ));
        }
        let revision = i64::try_from(revision).map_err(|_| {
            StorageError::invalid("receipt stream revision exceeds the SQLite range")
        })?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT actor_key, scope_key, request_id, command_digest \
                 FROM command_receipts WHERE stream_id = ?1 AND revision = ?2",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map(params![stream_id, revision], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        let [(actor_key, scope_key, request_id, command_digest)] = rows.as_slice() else {
            if rows.is_empty() {
                return Ok(None);
            }
            return Err(StorageError::adapter(
                "state stream revision has multiple durable command receipts",
            ));
        };
        let identity = ReceiptIdentity::new(
            ReceiptActorKey::from_encoded(actor_key.clone())?,
            ReceiptScopeKey::from_encoded(scope_key.clone())?,
            RequestId(request_id.clone()),
        )?;
        let command_digest = Sha256Digest(command_digest.clone());
        validate_sha256_digest(&command_digest)?;
        replay_stored_receipt(
            connection,
            &identity,
            &command_digest,
            StoredReceipt {
                command_digest: command_digest.0.clone(),
                stream_id: stream_id.to_owned(),
                revision,
            },
        )
        .map(Some)
    }

    fn load_pending_audit_event(
        &self,
        identity: &ReceiptIdentity,
    ) -> Result<Option<PendingAuditEvent>, StorageError> {
        load_pending_audit_event(self.connection()?, identity)
    }

    fn pending_audit_events(&self) -> Result<Vec<PendingAuditEvent>, StorageError> {
        pending_audit_events(self.connection()?)
    }

    fn mark_audit_event_persisted(&mut self, event_id: &str) -> Result<(), StorageError> {
        if event_id.is_empty() {
            return Err(StorageError::invalid_input(
                "pending audit event id must not be empty",
            ));
        }
        let changed = self
            .connection_mut()?
            .execute(
                "UPDATE audit_outbox SET persisted = 1 WHERE event_id = ?1",
                [event_id],
            )
            .map_err(sql_error)?;
        if changed != 1 {
            return Err(StorageError::adapter(
                "pending audit event acknowledgement has no durable row",
            ));
        }
        Ok(())
    }

    fn load_outbox_event(
        &self,
        event_id: &str,
    ) -> Result<Option<DurableOutboxEvent>, StorageError> {
        if event_id.is_empty() {
            return Err(StorageError::invalid_input("event_id must not be empty"));
        }
        let row = self
            .connection()?
            .query_row(
                "SELECT o.sequence, o.event_id, o.topic, o.payload, o.receipt_actor_key, \
                        o.receipt_scope_key, o.request_id, o.projection_stream_kind, \
                        o.projection_resource_id, o.projection_stream_sequence, \
                        o.public_scope_json, o.public_stream_json, \
                        o.public_occurred_at_json, o.public_source_json, \
                        r.command_digest, r.stream_id, r.revision \
                 FROM outbox o JOIN command_receipts r \
                   ON r.actor_key = o.receipt_actor_key \
                  AND r.scope_key = o.receipt_scope_key \
                  AND r.request_id = o.request_id \
                 WHERE o.event_id = ?1",
                [event_id],
                StoredDurableOutboxRow::from_sql_row,
            )
            .optional()
            .map_err(sql_error)?;
        row.map(|row| row.into_durable(self.connection()?))
            .transpose()
    }

    fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError> {
        load_state_from_connection(self.connection()?, stream_id)
    }

    fn last_state_stream_id(&self, prefix: &str) -> Result<Option<String>, StorageError> {
        validate_state_scan_prefix(prefix)?;
        self.connection()?
            .query_row(
                "SELECT MAX(stream_id) FROM product_state
                 WHERE substr(stream_id, 1, length(?1)) = ?1",
                [prefix],
                |row| row.get(0),
            )
            .map_err(sql_error)
    }

    fn scan_state_streams(
        &self,
        prefix: &str,
        after: &str,
        upper_bound: &str,
        limit: usize,
    ) -> Result<Vec<StoredState>, StorageError> {
        validate_state_scan(prefix, after, upper_bound, limit)?;
        let limit = i64::try_from(limit)
            .map_err(|_| StorageError::invalid_input("state stream scan limit is invalid"))?;
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT stream_id, revision, payload FROM product_state
                 WHERE substr(stream_id, 1, length(?1)) = ?1
                   AND stream_id > ?2 AND stream_id <= ?3
                 ORDER BY stream_id LIMIT ?4",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map(params![prefix, after, upper_bound, limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        rows.into_iter()
            .map(|(stream_id, revision, payload)| {
                Ok(StoredState {
                    stream_id,
                    revision: u64::try_from(revision)
                        .map_err(|_| StorageError::adapter("stored revision is negative"))?,
                    payload,
                })
            })
            .collect()
    }

    fn load_bounded_state_directory(
        &self,
        prefix: &str,
        max_entries: usize,
        max_payload_bytes: usize,
    ) -> Result<Vec<StoredStateDirectoryEntry>, StorageError> {
        load_bounded_state_directory_from_connection(
            self.connection()?,
            prefix,
            None,
            max_entries,
            max_payload_bytes,
        )
    }

    fn load_bounded_state_directory_at_revision(
        &self,
        prefix: &str,
        revision: u64,
        max_entries: usize,
        max_payload_bytes: usize,
    ) -> Result<Vec<StoredStateDirectoryEntry>, StorageError> {
        load_bounded_state_directory_from_connection(
            self.connection()?,
            prefix,
            Some(revision),
            max_entries,
            max_payload_bytes,
        )
    }

    fn load_projection_read_cut(
        &self,
        state_stream_ids: &[String],
        key: &ProjectionEventStreamKey,
        expected: Option<&ProjectionEventCursor>,
    ) -> Result<ProjectionReadCut, StorageError> {
        let mut connection = self.read_connection.lock().map_err(|_| {
            StorageError::adapter("SQLite projection read connection lock is poisoned")
        })?;
        let transaction = connection.transaction().map_err(sql_error)?;
        let states = state_stream_ids
            .iter()
            .map(|stream_id| load_state_from_connection(&transaction, stream_id))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        let projection_event_cursor =
            load_projection_event_cursor_from_connection(&transaction, key, expected)?;
        transaction.commit().map_err(sql_error)?;
        Ok(ProjectionReadCut::new(states, projection_event_cursor))
    }

    fn load_journal(
        &self,
        key: &AggregateJournalKey,
    ) -> Result<Option<LoadedAggregateJournal>, StorageError> {
        load_aggregate_journal(self.connection()?, key)
    }

    fn load_projection_event_cursor(
        &self,
        key: &ProjectionEventStreamKey,
        expected: Option<&ProjectionEventCursor>,
    ) -> Result<ProjectionEventCursor, StorageError> {
        load_projection_event_cursor_from_connection(self.connection()?, key, expected)
    }

    fn pending_events(&self) -> Result<Vec<OutboxEvent>, StorageError> {
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT sequence, event_id, topic, payload, receipt_actor_key, receipt_scope_key, \
                        projection_stream_kind, projection_resource_id, \
                        projection_stream_sequence, public_scope_json, public_stream_json, \
                        public_occurred_at_json, public_source_json FROM outbox \
                 WHERE published = 0 ORDER BY sequence ASC",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<Vec<u8>>>(9)?,
                    row.get::<_, Option<Vec<u8>>>(10)?,
                    row.get::<_, Option<Vec<u8>>>(11)?,
                    row.get::<_, Option<Vec<u8>>>(12)?,
                ))
            })
            .map_err(sql_error)?;
        rows.map(|row| {
            let (
                sequence,
                event_id,
                topic,
                payload,
                actor_key,
                scope_key,
                stream_kind,
                resource_id,
                stream_sequence,
                public_scope,
                public_stream,
                public_occurred_at,
                public_source,
            ) = row.map_err(sql_error)?;
            let receipt_actor_key = ReceiptActorKey::from_encoded(actor_key)?;
            let receipt_scope_key = ReceiptScopeKey::from_encoded(scope_key.clone())?;
            let projection_cursor = stored_projection_cursor(
                scope_key,
                stream_kind,
                resource_id,
                stream_sequence,
                &event_id,
            )?;
            let public_context = stored_public_context(
                projection_cursor.as_ref(),
                public_scope,
                public_stream,
                public_occurred_at,
                public_source,
                &receipt_scope_key,
                Some(&receipt_actor_key),
            )?;
            Ok(OutboxEvent {
                sequence: u64::try_from(sequence)
                    .map_err(|_| StorageError::adapter("outbox sequence is negative"))?,
                projection_cursor,
                public_context,
                event_id,
                topic,
                payload,
            })
        })
        .collect()
    }

    fn mark_published(&mut self, event_id: &str) -> Result<(), StorageError> {
        let changed = self
            .connection_mut()?
            .execute(
                "UPDATE outbox SET published = 1 WHERE event_id = ?1",
                [event_id],
            )
            .map_err(sql_error)?;
        if changed == 0 {
            return Err(StorageError::adapter(format!(
                "outbox event {event_id} does not exist"
            )));
        }
        Ok(())
    }

    fn close(self: Box<Self>) -> Result<(), StorageError> {
        let SqliteStorage {
            mut connection,
            read_connection,
            database_path: _,
        } = *self;
        let read_connection = read_connection.into_inner().map_err(|_| {
            StorageError::adapter("SQLite projection read connection lock is poisoned")
        })?;
        read_connection
            .close()
            .map_err(|(_, error)| sql_error(error))?;
        let connection = connection.take().ok_or_else(StorageError::closed)?;
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(sql_error)?;
        connection.close().map_err(|(_, error)| sql_error(error))?;
        Ok(())
    }
}

fn validate_state_execution_job_authority(
    commit: &StateCommit,
    submission: &ExecutionJobSubmission,
) -> Result<(), StorageError> {
    if commit.receipt_identity.request_id() != &submission.request_id {
        return Err(StorageError::invalid_input(
            "state and execution job request identities differ",
        ));
    }
    let PublicEventScope::Repository {
        organization_id,
        workspace_id,
        project_id,
        repository_id,
    } = repository_scope_from_receipt_key(commit.receipt_identity.scope_key())?
    else {
        return Err(StorageError::invalid_input(
            "state and execution job require a repository receipt scope",
        ));
    };
    if submission.scope.organization_id != organization_id
        || submission.scope.workspace_id != workspace_id
        || submission.scope.project_id != project_id
        || submission.scope.repository_id != repository_id
    {
        return Err(StorageError::invalid_input(
            "state and execution job repository scopes differ",
        ));
    }
    Ok(())
}

pub(crate) fn commit_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
) -> Result<CommitReceipt, StorageError> {
    commit_in_transaction_with_claim_authority(transaction, commit, false)
}

pub(crate) fn commit_claimed_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
) -> Result<CommitReceipt, StorageError> {
    commit_in_transaction_with_claim_authority(transaction, commit, true)
}

fn commit_in_transaction_with_claim_authority(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
    claim_authority_validated: bool,
) -> Result<CommitReceipt, StorageError> {
    if let Some(prior) = prior_receipt(transaction, &commit.receipt_identity)? {
        return replay_receipt(transaction, commit, prior);
    }
    if !claim_authority_validated && control_plane_command_claim_exists(transaction, commit)? {
        return Err(StorageError::invalid(
            "claimed Control Plane command requires its atomic instance fence",
        ));
    }
    if commit.receipt_replay_required {
        return Err(StorageError::request_replay_missing(
            commit.receipt_identity.request_id(),
        ));
    }

    check_state_revision_guards(transaction, commit)?;
    check_state_mutation_revisions(transaction, commit)?;
    append_state_commit(transaction, commit)
}

fn control_plane_command_claim_exists(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
) -> Result<bool, StorageError> {
    let table_exists = transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = 'control_plane_command_claims'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(sql_error)?;
    if !table_exists {
        return Ok(false);
    }
    transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM control_plane_command_claims
                 WHERE actor_key = ?1 AND scope_key = ?2 AND request_id = ?3
             )",
            params![
                commit.receipt_identity.actor_key().as_bytes(),
                commit.receipt_identity.scope_key().as_bytes(),
                commit.receipt_identity.request_id().0,
            ],
            |row| row.get::<_, bool>(0),
        )
        .map_err(sql_error)
}

fn load_bounded_state_directory_from_connection(
    connection: &Connection,
    prefix: &str,
    revision: Option<u64>,
    max_entries: usize,
    max_payload_bytes: usize,
) -> Result<Vec<StoredStateDirectoryEntry>, StorageError> {
    validate_state_scan_prefix(prefix)?;
    if max_entries == 0 || max_payload_bytes == 0 || revision == Some(0) {
        return Err(StorageError::invalid_input(
            "state directory revision and bounds must be positive",
        ));
    }
    let query_limit = max_entries
        .checked_add(1)
        .ok_or_else(|| StorageError::invalid_input("state directory entry bound is invalid"))?;
    let query_limit = i64::try_from(query_limit)
        .map_err(|_| StorageError::invalid_input("state directory entry bound is invalid"))?;
    let revision = revision
        .map(i64::try_from)
        .transpose()
        .map_err(|_| StorageError::invalid_input("state directory revision is invalid"))?;
    let mut statement = connection
        .prepare(
            "SELECT stream_id, revision, payload FROM product_state
             WHERE substr(stream_id, 1, length(?1)) = ?1
               AND (?2 IS NULL OR revision = ?2)
             ORDER BY stream_id LIMIT ?3",
        )
        .map_err(sql_error)?;
    let mut query = statement
        .query(params![prefix, revision, query_limit])
        .map_err(sql_error)?;
    let mut directory = Vec::with_capacity(max_entries.min(1_024));
    let mut total_payload_bytes = 0_usize;
    while let Some(row) = query.next().map_err(sql_error)? {
        if directory.len() == max_entries {
            return Err(StorageError::invalid_input(
                "state directory exceeds its bounded entry limit",
            ));
        }
        let payload = row.get::<_, Vec<u8>>(2).map_err(sql_error)?;
        total_payload_bytes = total_payload_bytes
            .checked_add(payload.len())
            .ok_or_else(|| {
                StorageError::invalid_input("state directory payload bound overflowed")
            })?;
        if total_payload_bytes > max_payload_bytes {
            return Err(StorageError::invalid_input(
                "state directory exceeds its bounded payload limit",
            ));
        }
        let revision = row.get::<_, i64>(1).map_err(sql_error)?;
        directory.push(StoredStateDirectoryEntry {
            stream_id: row.get(0).map_err(sql_error)?,
            revision: u64::try_from(revision)
                .map_err(|_| StorageError::adapter("stored revision is negative"))?,
            payload_sha256: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload))),
        });
    }
    Ok(directory)
}

fn load_state_from_connection(
    connection: &Connection,
    stream_id: &str,
) -> Result<Option<StoredState>, StorageError> {
    connection
        .query_row(
            "SELECT revision, payload FROM product_state WHERE stream_id = ?1",
            [stream_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()
        .map_err(sql_error)?
        .map(|(revision, payload)| {
            Ok(StoredState {
                stream_id: stream_id.to_owned(),
                revision: u64::try_from(revision)
                    .map_err(|_| StorageError::adapter("stored revision is negative"))?,
                payload,
            })
        })
        .transpose()
}

const MAX_STATE_STREAM_SCAN_LIMIT: usize = 512;

fn validate_state_scan_prefix(prefix: &str) -> Result<(), StorageError> {
    if prefix.is_empty() || prefix.len() > 200 || !prefix.is_ascii() || !portable_event_key(prefix)
    {
        return Err(StorageError::invalid_input(
            "state stream scan prefix is invalid",
        ));
    }
    Ok(())
}

fn validate_state_scan(
    prefix: &str,
    after: &str,
    upper_bound: &str,
    limit: usize,
) -> Result<(), StorageError> {
    validate_state_scan_prefix(prefix)?;
    if (!after.is_empty() && (!after.starts_with(prefix) || !portable_event_key(after)))
        || !upper_bound.starts_with(prefix)
        || !portable_event_key(upper_bound)
        || after > upper_bound
        || !(1..=MAX_STATE_STREAM_SCAN_LIMIT).contains(&limit)
    {
        return Err(StorageError::invalid_input(
            "state stream scan bounds are invalid",
        ));
    }
    Ok(())
}

fn load_projection_event_cursor_from_connection(
    connection: &Connection,
    key: &ProjectionEventStreamKey,
    expected: Option<&ProjectionEventCursor>,
) -> Result<ProjectionEventCursor, StorageError> {
    key.stream().validate()?;
    if let Some(expected) = expected {
        if expected.key() != key {
            return Err(StorageError::invalid(
                "projection event cursor belongs to another scope or stream",
            ));
        }
        if expected.sequence() == 0 {
            return ProjectionEventCursor::try_new(key.clone(), 0, None);
        }
        let stored_event_id = connection
            .query_row(
                "SELECT event_id FROM outbox \
                 WHERE receipt_scope_key = ?1 AND projection_stream_kind = ?2 \
                   AND projection_resource_id = ?3 AND projection_stream_sequence = ?4",
                params![
                    key.scope_key().as_bytes(),
                    key.stream().kind(),
                    key.stream().resource_id(),
                    i64::try_from(expected.sequence()).map_err(|_| {
                        StorageError::invalid("projection event sequence is out of range")
                    })?,
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_error)?;
        let Some(stored_event_id) = stored_event_id else {
            let Some((head_sequence, _)) = load_projection_stream_head(connection, key)? else {
                return Err(StorageError::invalid(
                    "projection event cursor was never issued for this stream",
                ));
            };
            if expected.sequence() > head_sequence {
                return Err(StorageError::invalid(
                    "projection event cursor is beyond the durable stream head",
                ));
            }
            return Err(StorageError::event_cursor_expired());
        };
        if expected.event_id().map(|event_id| event_id.0.as_str()) != Some(stored_event_id.as_str())
        {
            return Err(StorageError::invalid(
                "projection event cursor eventId does not match its stored position",
            ));
        }
        return ProjectionEventCursor::try_new(
            key.clone(),
            expected.sequence(),
            expected.event_id().cloned(),
        );
    }

    match load_projection_stream_head(connection, key)? {
        Some((sequence, event_id)) => ProjectionEventCursor::try_new(
            key.clone(),
            sequence,
            Some(ControlPlaneEventId(event_id)),
        ),
        None => ProjectionEventCursor::try_new(key.clone(), 0, None),
    }
}

struct StoredReceipt {
    command_digest: String,
    stream_id: String,
    revision: i64,
}

fn prior_receipt(
    connection: &Connection,
    identity: &ReceiptIdentity,
) -> Result<Option<StoredReceipt>, StorageError> {
    connection
        .query_row(
            "SELECT command_digest, stream_id, revision FROM command_receipts \
             WHERE actor_key = ?1 AND scope_key = ?2 AND request_id = ?3",
            params![
                identity.actor_key().as_bytes(),
                identity.scope_key().as_bytes(),
                identity.request_id().0,
            ],
            |row| {
                Ok(StoredReceipt {
                    command_digest: row.get(0)?,
                    stream_id: row.get(1)?,
                    revision: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(sql_error)
}

fn load_pending_audit_event(
    connection: &Connection,
    identity: &ReceiptIdentity,
) -> Result<Option<PendingAuditEvent>, StorageError> {
    connection
        .query_row(
            "SELECT event_id, payload FROM audit_outbox \
             WHERE actor_key = ?1 AND scope_key = ?2 AND request_id = ?3",
            params![
                identity.actor_key().as_bytes(),
                identity.scope_key().as_bytes(),
                identity.request_id().0,
            ],
            |row| {
                let event_id = row.get::<_, String>(0)?;
                let payload = row.get::<_, Vec<u8>>(1)?;
                Ok((event_id, payload))
            },
        )
        .optional()
        .map_err(sql_error)?
        .map(|(event_id, payload)| PendingAuditEvent::new(event_id, payload))
        .transpose()
}

fn pending_audit_events(connection: &Connection) -> Result<Vec<PendingAuditEvent>, StorageError> {
    let mut statement = connection
        .prepare(
            "SELECT event_id, payload FROM audit_outbox \
             WHERE persisted = 0 ORDER BY event_id",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(sql_error)?;
    rows.map(|row| {
        let (event_id, payload) = row.map_err(sql_error)?;
        PendingAuditEvent::new(event_id, payload)
    })
    .collect()
}

fn replay_receipt(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
    prior: StoredReceipt,
) -> Result<CommitReceipt, StorageError> {
    if load_pending_audit_event(transaction, &commit.receipt_identity)?
        != commit.pending_audit_event.clone()
    {
        return Err(StorageError::request_conflict(
            commit.receipt_identity.request_id(),
        ));
    }
    replay_stored_receipt(
        transaction,
        &commit.receipt_identity,
        &commit.command_digest,
        prior,
    )
}

fn replay_stored_receipt(
    connection: &Connection,
    identity: &ReceiptIdentity,
    command_digest: &Sha256Digest,
    prior: StoredReceipt,
) -> Result<CommitReceipt, StorageError> {
    if prior.command_digest != command_digest.0 {
        return Err(StorageError::request_conflict(identity.request_id()));
    }
    Ok(CommitReceipt {
        receipt_identity: identity.clone(),
        command_digest: command_digest.clone(),
        stream_id: prior.stream_id,
        revision: u64::try_from(prior.revision)
            .map_err(|_| StorageError::adapter("stored revision is negative"))?,
        events: receipt_events(connection, identity)?,
        idempotent_replay: true,
    })
}

fn check_state_revision_guards(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
) -> Result<(), StorageError> {
    for guard in &commit.state_guards {
        let actual_revision = stored_state_revision(transaction, &guard.stream_id)?;
        if actual_revision != guard.expected_revision {
            return Err(StorageError::state_revision_guard_conflict(
                &guard.stream_id,
                guard.expected_revision,
                actual_revision,
            ));
        }
    }
    Ok(())
}

fn check_state_mutation_revisions(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
) -> Result<(), StorageError> {
    for mutation in &commit.state_mutations {
        let actual_revision = stored_state_revision(transaction, &mutation.stream_id)?;
        if actual_revision != mutation.expected_revision {
            return Err(StorageError::state_mutation_revision_conflict(
                &mutation.stream_id,
                mutation.expected_revision,
                actual_revision,
            ));
        }
    }
    Ok(())
}

fn stored_state_revision(
    transaction: &rusqlite::Transaction<'_>,
    stream_id: &str,
) -> Result<u64, StorageError> {
    let revision = transaction
        .query_row(
            "SELECT revision FROM product_state WHERE stream_id = ?1",
            [stream_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(sql_error)?
        .unwrap_or(0);
    u64::try_from(revision).map_err(|_| StorageError::adapter("stored revision is negative"))
}

fn append_state_commit(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
) -> Result<CommitReceipt, StorageError> {
    let actual_revision = stored_state_revision(transaction, &commit.stream_id)?;
    if actual_revision != commit.expected_revision {
        return Err(StorageError::revision_conflict(
            commit.expected_revision,
            actual_revision,
        ));
    }

    let expected_revision = i64::try_from(commit.expected_revision)
        .map_err(|_| StorageError::invalid("expected_revision is out of range"))?;
    let revision = expected_revision
        .checked_add(1)
        .ok_or_else(|| StorageError::invalid("revision is out of range"))?;
    append_state(transaction, commit, revision)?;
    append_state_mutations(transaction, commit)?;
    if let Some(publication) = commit.journal_publication() {
        append_journal_publication(transaction, publication)?;
    }
    append_receipt(transaction, commit, revision)?;
    if let Some(pending_audit_event) = commit.pending_audit_event() {
        append_pending_audit_event(transaction, commit, pending_audit_event)?;
    }
    append_outbox_events(transaction, commit)?;

    Ok(CommitReceipt {
        receipt_identity: commit.receipt_identity.clone(),
        command_digest: commit.command_digest.clone(),
        stream_id: commit.stream_id.clone(),
        revision: u64::try_from(revision)
            .map_err(|_| StorageError::adapter("committed revision is negative"))?,
        events: receipt_events(transaction, &commit.receipt_identity)?,
        idempotent_replay: false,
    })
}

fn append_pending_audit_event(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
    pending_audit_event: &PendingAuditEvent,
) -> Result<(), StorageError> {
    transaction
        .execute(
            "INSERT INTO audit_outbox \
             (actor_key, scope_key, request_id, event_id, payload, persisted) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![
                commit.receipt_identity.actor_key().as_bytes(),
                commit.receipt_identity.scope_key().as_bytes(),
                commit.receipt_identity.request_id().0,
                pending_audit_event.event_id(),
                pending_audit_event.payload(),
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn append_journal_publication(
    transaction: &rusqlite::Transaction<'_>,
    publication: &AggregateJournalPublication,
) -> Result<(), StorageError> {
    match publication {
        AggregateJournalPublication::Create {
            key,
            manifest,
            first_record,
        } => {
            let exists = transaction
                .query_row(
                    "SELECT 1 FROM aggregate_journals \
                     WHERE aggregate_type = ?1 AND aggregate_id = ?2",
                    params![key.aggregate_type(), key.aggregate_id()],
                    |_| Ok(()),
                )
                .optional()
                .map_err(sql_error)?
                .is_some();
            if exists {
                return Err(StorageError::journal_already_exists(key));
            }
            transaction
                .execute(
                    "INSERT INTO aggregate_journals \
                     (aggregate_type, aggregate_id, manifest) VALUES (?1, ?2, ?3)",
                    params![key.aggregate_type(), key.aggregate_id(), manifest],
                )
                .map_err(sql_error)?;
            insert_journal_record(transaction, key, first_record)?;
        }
        AggregateJournalPublication::Append {
            key,
            expected_tail_sequence,
            expected_tail_digest,
            record,
        } => {
            let tail = transaction
                .query_row(
                    "SELECT sequence, digest FROM aggregate_journal_records \
                     WHERE aggregate_type = ?1 AND aggregate_id = ?2 \
                     ORDER BY sequence DESC LIMIT 1",
                    params![key.aggregate_type(), key.aggregate_id()],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(sql_error)?;
            let Some((tail_sequence, tail_digest)) = tail else {
                return Err(StorageError::journal_not_found(key));
            };
            let expected_sequence = i64::try_from(*expected_tail_sequence)
                .map_err(|_| StorageError::invalid("expected journal tail is out of range"))?;
            if tail_sequence != expected_sequence || tail_digest != *expected_tail_digest {
                return Err(StorageError::journal_conflict(key));
            }
            insert_journal_record(transaction, key, record)?;
        }
    }
    Ok(())
}

fn insert_journal_record(
    transaction: &rusqlite::Transaction<'_>,
    key: &AggregateJournalKey,
    record: &AggregateJournalRecord,
) -> Result<(), StorageError> {
    let sequence = i64::try_from(record.sequence)
        .map_err(|_| StorageError::invalid("journal sequence is out of range"))?;
    transaction
        .execute(
            "INSERT INTO aggregate_journal_records \
             (aggregate_type, aggregate_id, sequence, digest, payload) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                key.aggregate_type(),
                key.aggregate_id(),
                sequence,
                record.digest,
                record.payload,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn append_state(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
    revision: i64,
) -> Result<(), StorageError> {
    upsert_state(transaction, &commit.stream_id, revision, &commit.state)
}

fn append_state_mutations(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
) -> Result<(), StorageError> {
    for mutation in &commit.state_mutations {
        let expected_revision = i64::try_from(mutation.expected_revision)
            .map_err(|_| StorageError::invalid("state mutation revision is out of range"))?;
        let revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| StorageError::invalid("state mutation revision is out of range"))?;
        upsert_state(transaction, &mutation.stream_id, revision, &mutation.state)?;
    }
    Ok(())
}

fn upsert_state(
    transaction: &rusqlite::Transaction<'_>,
    stream_id: &str,
    revision: i64,
    state: &[u8],
) -> Result<(), StorageError> {
    transaction
        .execute(
            "INSERT INTO product_state (stream_id, revision, payload) VALUES (?1, ?2, ?3) \
             ON CONFLICT(stream_id) DO UPDATE SET revision = excluded.revision, payload = excluded.payload",
            params![stream_id, revision, state],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn append_outbox_events(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
) -> Result<(), StorageError> {
    for event in &commit.events {
        let stream_position = event
            .projection_stream()
            .map(|stream| next_projection_stream_position(transaction, commit, stream))
            .transpose()?;
        let (public_scope, public_stream, public_occurred_at, public_source) = event
            .public_context()
            .map(serialize_public_context)
            .transpose()?
            .map_or(
                (None, None, None, None),
                |(scope, stream, occurred_at, source)| {
                    (Some(scope), Some(stream), Some(occurred_at), Some(source))
                },
            );
        transaction
            .execute(
                "INSERT INTO outbox \
                 (event_id, receipt_actor_key, receipt_scope_key, request_id, topic, payload, \
                  published, projection_stream_kind, projection_resource_id, \
                  projection_stream_sequence, public_scope_json, public_stream_json, \
                  public_occurred_at_json, public_source_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    event.event_id,
                    commit.receipt_identity.actor_key().as_bytes(),
                    commit.receipt_identity.scope_key().as_bytes(),
                    commit.receipt_identity.request_id().0,
                    event.topic,
                    event.payload,
                    event.projection_stream().map(ProjectionEventStream::kind),
                    event
                        .projection_stream()
                        .map(ProjectionEventStream::resource_id),
                    stream_position,
                    public_scope,
                    public_stream,
                    public_occurred_at,
                    public_source,
                ],
            )
            .map_err(sql_error)?;
        if let (Some(stream), Some(position)) = (event.projection_stream(), stream_position) {
            transaction
                .execute(
                    "INSERT INTO projection_event_stream_heads \
                     (scope_key, stream_kind, resource_id, sequence, event_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(scope_key, stream_kind, resource_id) DO UPDATE SET \
                         sequence = excluded.sequence, event_id = excluded.event_id",
                    params![
                        commit.receipt_identity.scope_key().as_bytes(),
                        stream.kind(),
                        stream.resource_id(),
                        position,
                        event.event_id,
                    ],
                )
                .map_err(sql_error)?;
        }
    }
    Ok(())
}

type SerializedPublicContext = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>);

struct StoredDurableOutboxRow {
    sequence: i64,
    event_id: String,
    topic: String,
    payload: Vec<u8>,
    actor_key: Vec<u8>,
    scope_key: Vec<u8>,
    request_id: String,
    stream_kind: Option<String>,
    resource_id: Option<String>,
    stream_sequence: Option<i64>,
    public_scope: Option<Vec<u8>>,
    public_stream: Option<Vec<u8>>,
    public_occurred_at: Option<Vec<u8>>,
    public_source: Option<Vec<u8>>,
    command_digest: String,
    stream_id: String,
    revision: i64,
}

impl StoredDurableOutboxRow {
    fn from_sql_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            sequence: row.get(0)?,
            event_id: row.get(1)?,
            topic: row.get(2)?,
            payload: row.get(3)?,
            actor_key: row.get(4)?,
            scope_key: row.get(5)?,
            request_id: row.get(6)?,
            stream_kind: row.get(7)?,
            resource_id: row.get(8)?,
            stream_sequence: row.get(9)?,
            public_scope: row.get(10)?,
            public_stream: row.get(11)?,
            public_occurred_at: row.get(12)?,
            public_source: row.get(13)?,
            command_digest: row.get(14)?,
            stream_id: row.get(15)?,
            revision: row.get(16)?,
        })
    }

    fn into_durable(self, connection: &Connection) -> Result<DurableOutboxEvent, StorageError> {
        let receipt_scope_key = ReceiptScopeKey::from_encoded(self.scope_key.clone())?;
        let receipt_actor_key = ReceiptActorKey::from_encoded(self.actor_key)?;
        let projection_cursor = stored_projection_cursor(
            self.scope_key,
            self.stream_kind,
            self.resource_id,
            self.stream_sequence,
            &self.event_id,
        )?;
        let public_context = stored_public_context(
            projection_cursor.as_ref(),
            self.public_scope,
            self.public_stream,
            self.public_occurred_at,
            self.public_source,
            &receipt_scope_key,
            Some(&receipt_actor_key),
        )?;
        let event = OutboxEvent {
            sequence: u64::try_from(self.sequence)
                .map_err(|_| StorageError::adapter("outbox sequence is negative"))?,
            event_id: self.event_id,
            topic: self.topic,
            payload: self.payload,
            projection_cursor,
            public_context,
        };
        let receipt_identity = ReceiptIdentity::new(
            receipt_actor_key,
            receipt_scope_key,
            RequestId(self.request_id),
        )?;
        require_event_owned_by_receipt(connection, &receipt_identity, &event)?;
        let command_digest = Sha256Digest(self.command_digest);
        validate_sha256_digest(&command_digest)?;
        Ok(DurableOutboxEvent {
            event,
            receipt_identity,
            command_digest,
            stream_id: self.stream_id,
            revision: u64::try_from(self.revision)
                .map_err(|_| StorageError::adapter("stored receipt revision is negative"))?,
        })
    }
}

fn require_event_owned_by_receipt(
    connection: &Connection,
    identity: &ReceiptIdentity,
    event: &OutboxEvent,
) -> Result<(), StorageError> {
    let events = receipt_events(connection, identity)?;
    let matching = events
        .iter()
        .filter(|candidate| candidate.event_id == event.event_id)
        .collect::<Vec<_>>();
    let [receipt_event] = matching.as_slice() else {
        return Err(StorageError::adapter(
            "durable outbox event is not owned exactly once by its receipt",
        ));
    };
    if *receipt_event != event {
        return Err(StorageError::adapter(
            "durable outbox event differs from its receipt event",
        ));
    }
    Ok(())
}

fn serialize_public_context(
    context: &PublicProjectionEventContext,
) -> Result<SerializedPublicContext, StorageError> {
    Ok((
        serde_json::to_vec(context.scope()).map_err(|error| json_error(&error))?,
        serde_json::to_vec(context.stream()).map_err(|error| json_error(&error))?,
        serde_json::to_vec(context.occurred_at()).map_err(|error| json_error(&error))?,
        serde_json::to_vec(context.source()).map_err(|error| json_error(&error))?,
    ))
}

fn json_error(error: &serde_json::Error) -> StorageError {
    StorageError::adapter(format!("public event context JSON failed: {error}"))
}

fn next_projection_stream_position(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
    stream: &ProjectionEventStream,
) -> Result<i64, StorageError> {
    let current = transaction
        .query_row(
            "SELECT sequence FROM projection_event_stream_heads \
             WHERE scope_key = ?1 AND stream_kind = ?2 AND resource_id = ?3",
            params![
                commit.receipt_identity.scope_key().as_bytes(),
                stream.kind(),
                stream.resource_id(),
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(sql_error)?
        .unwrap_or(0);
    if !(0..9_007_199_254_740_991).contains(&current) {
        return Err(StorageError::adapter(
            "projection event stream sequence is outside the public range",
        ));
    }
    current
        .checked_add(1)
        .ok_or_else(|| StorageError::adapter("projection event stream sequence overflow"))
}

fn load_projection_stream_head(
    connection: &Connection,
    key: &ProjectionEventStreamKey,
) -> Result<Option<(u64, String)>, StorageError> {
    connection
        .query_row(
            "SELECT sequence, event_id FROM projection_event_stream_heads \
             WHERE scope_key = ?1 AND stream_kind = ?2 AND resource_id = ?3",
            params![
                key.scope_key().as_bytes(),
                key.stream().kind(),
                key.stream().resource_id(),
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error)?
        .map(|(sequence, event_id)| {
            u64::try_from(sequence)
                .map(|sequence| (sequence, event_id))
                .map_err(|_| StorageError::adapter("event sequence is negative"))
        })
        .transpose()
}

fn append_receipt(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
    revision: i64,
) -> Result<(), StorageError> {
    transaction
        .execute(
            "INSERT INTO command_receipts \
             (actor_key, scope_key, request_id, command_digest, stream_id, revision) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                commit.receipt_identity.actor_key().as_bytes(),
                commit.receipt_identity.scope_key().as_bytes(),
                commit.receipt_identity.request_id().0,
                commit.command_digest.0,
                commit.stream_id,
                revision,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn apply_migrations(connection: &mut Connection) -> Result<(), StorageError> {
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(sql_error)?;
    if !matches!(version, 0 | 1 | 2 | 3 | 4 | 5 | SCHEMA_VERSION) {
        return Err(StorageError::adapter(format!(
            "unsupported schema version {version}"
        )));
    }

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    match version {
        0 | SCHEMA_VERSION => create_schema_v6(&transaction)?,
        1 => {
            migrate_v1_to_v2(&transaction)?;
            create_aggregate_journal_schema(&transaction)?;
            create_projection_event_schema(&transaction)?;
            create_public_event_context_schema(&transaction)?;
        }
        2 => {
            create_aggregate_journal_schema(&transaction)?;
            create_projection_event_schema(&transaction)?;
            create_public_event_context_schema(&transaction)?;
        }
        3 => {
            create_projection_event_schema(&transaction)?;
            create_public_event_context_schema(&transaction)?;
        }
        4 => {
            create_public_event_context_schema(&transaction)?;
            migrate_projection_event_stream_heads_to_v6(&transaction)?;
        }
        5 => migrate_projection_event_stream_heads_to_v6(&transaction)?,
        unsupported => {
            return Err(StorageError::adapter(format!(
                "unsupported schema version {unsupported}"
            )));
        }
    }
    create_pending_audit_event_schema(&transaction)?;
    git_candidate_retention::create_schema(&transaction)?;
    validate_journal_schema(&transaction)?;
    validate_projection_event_schema(&transaction)?;
    validate_public_event_context_schema(&transaction)?;
    validate_pending_audit_event_schema(&transaction)?;
    git_candidate_retention::validate_schema(&transaction)?;
    transaction
        .pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(sql_error)?;
    transaction.commit().map_err(sql_error)?;

    let migrated_version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(sql_error)?;
    if migrated_version != SCHEMA_VERSION {
        return Err(StorageError::adapter(format!(
            "unsupported schema version {migrated_version}"
        )));
    }
    Ok(())
}

fn validate_journal_schema(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    let journal_columns = {
        let mut statement = transaction
            .prepare("PRAGMA table_info(aggregate_journals)")
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)?
    };
    if journal_columns != ["aggregate_type", "aggregate_id", "manifest"] {
        return Err(StorageError::adapter(
            "aggregate journal schema is not canonical",
        ));
    }
    let record_columns = {
        let mut statement = transaction
            .prepare("PRAGMA table_info(aggregate_journal_records)")
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)?
    };
    if record_columns
        != [
            "aggregate_type",
            "aggregate_id",
            "sequence",
            "digest",
            "payload",
        ]
    {
        return Err(StorageError::adapter(
            "aggregate journal record schema is not canonical",
        ));
    }
    Ok(())
}

fn create_schema_v3(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    create_schema_v2(transaction)?;
    create_aggregate_journal_schema(transaction)
}

fn create_schema_v4(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    create_schema_v3(transaction)?;
    create_projection_event_schema(transaction)
}

fn create_schema_v5(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    create_schema_v4(transaction)?;
    create_public_event_context_schema(transaction)
}

fn create_schema_v6(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    create_schema_v5(transaction)
}

fn create_public_event_context_schema(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StorageError> {
    let columns = table_columns(transaction, "outbox")?;
    if !columns.contains(&"public_scope_json".to_owned()) {
        let unbound_public_events = transaction
            .query_row(
                "SELECT COUNT(*) FROM outbox WHERE projection_stream_kind IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(sql_error)?;
        if unbound_public_events != 0 {
            return Err(StorageError::adapter(
                "legacy public outbox rows have no durable envelope context",
            ));
        }
        transaction
            .execute_batch(
                "ALTER TABLE outbox ADD COLUMN public_scope_json BLOB;
                 ALTER TABLE outbox ADD COLUMN public_stream_json BLOB;
                 ALTER TABLE outbox ADD COLUMN public_occurred_at_json BLOB;
                 ALTER TABLE outbox ADD COLUMN public_source_json BLOB;",
            )
            .map_err(sql_error)?;
    }
    Ok(())
}

fn validate_public_event_context_schema(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StorageError> {
    let columns = table_columns(transaction, "outbox")?;
    for required in [
        "public_scope_json",
        "public_stream_json",
        "public_occurred_at_json",
        "public_source_json",
    ] {
        if !columns.iter().any(|column| column == required) {
            return Err(StorageError::adapter(
                "public event context schema is not canonical",
            ));
        }
    }
    Ok(())
}

fn create_projection_event_schema(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StorageError> {
    let columns = table_columns(transaction, "outbox")?;
    if !columns.contains(&"projection_stream_kind".to_owned()) {
        transaction
            .execute_batch(
                "ALTER TABLE outbox ADD COLUMN projection_stream_kind TEXT;
                 ALTER TABLE outbox ADD COLUMN projection_resource_id TEXT;
                 ALTER TABLE outbox ADD COLUMN projection_stream_sequence INTEGER;",
            )
            .map_err(sql_error)?;
    }
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS projection_event_stream_heads (
                 scope_key BLOB NOT NULL,
                 stream_kind TEXT NOT NULL CHECK (
                     stream_kind IN ('scope', 'delivery', 'product-session', 'lease')
                 ),
                 resource_id TEXT NOT NULL,
                 sequence INTEGER NOT NULL CHECK (sequence > 0),
                 event_id TEXT NOT NULL,
                 PRIMARY KEY (scope_key, stream_kind, resource_id),
                 UNIQUE (scope_key, stream_kind, resource_id, sequence),
                 FOREIGN KEY (event_id) REFERENCES outbox (event_id)
             );
             CREATE UNIQUE INDEX IF NOT EXISTS outbox_projection_stream_sequence
                 ON outbox (receipt_scope_key, projection_stream_kind,
                            projection_resource_id, projection_stream_sequence)
                 WHERE projection_stream_kind IS NOT NULL;",
        )
        .map_err(sql_error)
}

fn migrate_projection_event_stream_heads_to_v6(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StorageError> {
    transaction
        .execute_batch(
            "CREATE TABLE projection_event_stream_heads_v6 (
                 scope_key BLOB NOT NULL,
                 stream_kind TEXT NOT NULL CHECK (
                     stream_kind IN ('scope', 'delivery', 'product-session', 'lease')
                 ),
                 resource_id TEXT NOT NULL,
                 sequence INTEGER NOT NULL CHECK (sequence > 0),
                 event_id TEXT NOT NULL,
                 PRIMARY KEY (scope_key, stream_kind, resource_id),
                 UNIQUE (scope_key, stream_kind, resource_id, sequence),
                 FOREIGN KEY (event_id) REFERENCES outbox (event_id)
             );
             INSERT INTO projection_event_stream_heads_v6
                 (scope_key, stream_kind, resource_id, sequence, event_id)
                 SELECT scope_key, stream_kind, resource_id, sequence, event_id
                 FROM projection_event_stream_heads;
             DROP TABLE projection_event_stream_heads;
             ALTER TABLE projection_event_stream_heads_v6
                 RENAME TO projection_event_stream_heads;",
        )
        .map_err(sql_error)
}

fn validate_projection_event_schema(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StorageError> {
    let columns = table_columns(transaction, "outbox")?;
    let required = [
        "projection_stream_kind",
        "projection_resource_id",
        "projection_stream_sequence",
    ];
    if !required
        .iter()
        .all(|column| columns.iter().any(|value| value == column))
    {
        return Err(StorageError::adapter(
            "projection event outbox schema is not canonical",
        ));
    }
    let head_columns = table_columns(transaction, "projection_event_stream_heads")?;
    if head_columns
        != [
            "scope_key",
            "stream_kind",
            "resource_id",
            "sequence",
            "event_id",
        ]
    {
        return Err(StorageError::adapter(
            "projection event stream head schema is not canonical",
        ));
    }
    let head_schema = transaction
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'table' AND name = 'projection_event_stream_heads'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map_err(sql_error)?;
    for stream_kind in ["'scope'", "'delivery'", "'product-session'", "'lease'"] {
        if !head_schema.contains(stream_kind) {
            return Err(StorageError::adapter(
                "projection event stream kinds are not canonical",
            ));
        }
    }
    Ok(())
}

fn create_pending_audit_event_schema(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StorageError> {
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS audit_outbox (
                 actor_key BLOB NOT NULL,
                 scope_key BLOB NOT NULL,
                 request_id TEXT NOT NULL,
                 event_id TEXT UNIQUE NOT NULL,
                 payload BLOB NOT NULL,
                 persisted INTEGER NOT NULL DEFAULT 0 CHECK (persisted IN (0, 1)),
                 PRIMARY KEY (actor_key, scope_key, request_id),
                 FOREIGN KEY (actor_key, scope_key, request_id)
                     REFERENCES command_receipts (actor_key, scope_key, request_id)
                     DEFERRABLE INITIALLY DEFERRED
             );",
        )
        .map_err(sql_error)
}

fn validate_pending_audit_event_schema(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StorageError> {
    let columns = table_columns(transaction, "audit_outbox")?;
    if columns
        != [
            "actor_key",
            "scope_key",
            "request_id",
            "event_id",
            "payload",
            "persisted",
        ]
    {
        return Err(StorageError::adapter(
            "pending audit event schema is not canonical",
        ));
    }
    Ok(())
}

fn table_columns(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
) -> Result<Vec<String>, StorageError> {
    let pragma = match table {
        "outbox" => "PRAGMA table_info(outbox)",
        "projection_event_stream_heads" => "PRAGMA table_info(projection_event_stream_heads)",
        "audit_outbox" => "PRAGMA table_info(audit_outbox)",
        _ => return Err(StorageError::adapter("unknown schema table")),
    };
    let mut statement = transaction.prepare(pragma).map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(sql_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
}

fn create_schema_v2(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS product_state (
                 stream_id TEXT PRIMARY KEY NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0),
                 payload BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS command_receipts (
                 actor_key BLOB NOT NULL,
                 scope_key BLOB NOT NULL,
                 request_id TEXT NOT NULL,
                 command_digest TEXT NOT NULL,
                 stream_id TEXT NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0),
                 PRIMARY KEY (actor_key, scope_key, request_id)
             );
             CREATE TABLE IF NOT EXISTS outbox (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_id TEXT UNIQUE NOT NULL,
                 receipt_actor_key BLOB NOT NULL,
                 receipt_scope_key BLOB NOT NULL,
                 request_id TEXT NOT NULL,
                 topic TEXT NOT NULL,
                 payload BLOB NOT NULL,
                 published INTEGER NOT NULL DEFAULT 0 CHECK (published IN (0, 1)),
                 FOREIGN KEY (receipt_actor_key, receipt_scope_key, request_id)
                     REFERENCES command_receipts (actor_key, scope_key, request_id)
                     DEFERRABLE INITIALLY DEFERRED
             );
             CREATE INDEX IF NOT EXISTS outbox_pending_sequence
                 ON outbox (published, sequence);",
        )
        .map_err(sql_error)
}

fn create_aggregate_journal_schema(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StorageError> {
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS aggregate_journals (
                 aggregate_type TEXT NOT NULL,
                 aggregate_id TEXT NOT NULL,
                 manifest BLOB NOT NULL,
                 PRIMARY KEY (aggregate_type, aggregate_id)
             );
             CREATE TABLE IF NOT EXISTS aggregate_journal_records (
                 aggregate_type TEXT NOT NULL,
                 aggregate_id TEXT NOT NULL,
                 sequence INTEGER NOT NULL CHECK (sequence > 0),
                 digest TEXT NOT NULL,
                 payload BLOB NOT NULL,
                 PRIMARY KEY (aggregate_type, aggregate_id, sequence),
                 FOREIGN KEY (aggregate_type, aggregate_id)
                     REFERENCES aggregate_journals (aggregate_type, aggregate_id)
                     ON DELETE CASCADE
             );",
        )
        .map_err(sql_error)
}

fn migrate_v1_to_v2(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    transaction
        .execute_batch(
            "ALTER TABLE command_receipts RENAME TO command_receipts_v1;
             ALTER TABLE outbox RENAME TO outbox_v1;",
        )
        .map_err(sql_error)?;
    create_schema_v2(transaction)?;

    let legacy_receipts = {
        let mut statement = transaction
            .prepare(
                "SELECT request_id, command_signature, stream_id, revision \
                 FROM command_receipts_v1 ORDER BY request_id",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)?
    };
    for (request_id, signature, stream_id, revision) in legacy_receipts {
        transaction
            .execute(
                "INSERT INTO command_receipts \
                 (actor_key, scope_key, request_id, command_digest, stream_id, revision) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    LEGACY_V1_ACTOR_KEY,
                    LEGACY_V1_SCOPE_KEY,
                    request_id,
                    sha256_digest(&signature).0,
                    stream_id,
                    revision,
                ],
            )
            .map_err(sql_error)?;
    }

    transaction
        .execute(
            "INSERT INTO outbox \
             (sequence, event_id, receipt_actor_key, receipt_scope_key, request_id, topic, payload, published) \
             SELECT sequence, event_id, ?1, ?2, request_id, topic, payload, published \
             FROM outbox_v1 ORDER BY sequence",
            params![LEGACY_V1_ACTOR_KEY, LEGACY_V1_SCOPE_KEY],
        )
        .map_err(sql_error)?;
    transaction
        .execute_batch(
            "DROP TABLE outbox_v1;
             DROP TABLE command_receipts_v1;
             CREATE INDEX IF NOT EXISTS outbox_pending_sequence
                 ON outbox (published, sequence);",
        )
        .map_err(sql_error)?;
    Ok(())
}

fn receipt_events(
    connection: &Connection,
    identity: &ReceiptIdentity,
) -> Result<Vec<OutboxEvent>, StorageError> {
    let mut statement = connection
        .prepare(
            "SELECT sequence, event_id, topic, payload, receipt_scope_key, \
                    projection_stream_kind, projection_resource_id, \
                    projection_stream_sequence, public_scope_json, public_stream_json, \
                    public_occurred_at_json, public_source_json FROM outbox \
             WHERE receipt_actor_key = ?1 AND receipt_scope_key = ?2 AND request_id = ?3 \
             ORDER BY sequence",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map(
            params![
                identity.actor_key().as_bytes(),
                identity.scope_key().as_bytes(),
                identity.request_id().0,
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<Vec<u8>>>(8)?,
                    row.get::<_, Option<Vec<u8>>>(9)?,
                    row.get::<_, Option<Vec<u8>>>(10)?,
                    row.get::<_, Option<Vec<u8>>>(11)?,
                ))
            },
        )
        .map_err(sql_error)?;
    rows.map(|row| {
        let (
            sequence,
            event_id,
            topic,
            payload,
            scope_key,
            stream_kind,
            resource_id,
            stream_sequence,
            public_scope,
            public_stream,
            public_occurred_at,
            public_source,
        ) = row.map_err(sql_error)?;
        let projection_cursor = stored_projection_cursor(
            scope_key,
            stream_kind,
            resource_id,
            stream_sequence,
            &event_id,
        )?;
        let public_context = stored_public_context(
            projection_cursor.as_ref(),
            public_scope,
            public_stream,
            public_occurred_at,
            public_source,
            identity.scope_key(),
            Some(identity.actor_key()),
        )?;
        Ok(OutboxEvent {
            sequence: u64::try_from(sequence)
                .map_err(|_| StorageError::adapter("outbox sequence is negative"))?,
            projection_cursor,
            public_context,
            event_id,
            topic,
            payload,
        })
    })
    .collect()
}

fn stored_projection_cursor(
    scope_key: Vec<u8>,
    stream_kind: Option<String>,
    resource_id: Option<String>,
    stream_sequence: Option<i64>,
    event_id: &str,
) -> Result<Option<ProjectionEventCursor>, StorageError> {
    match (stream_kind, resource_id, stream_sequence) {
        (None, None, None) => Ok(None),
        (Some(stream_kind), Some(resource_id), Some(stream_sequence)) => {
            let stream = ProjectionEventStream::from_stored(&stream_kind, resource_id)?;
            let key =
                ProjectionEventStreamKey::new(ReceiptScopeKey::from_encoded(scope_key)?, stream)?;
            ProjectionEventCursor::try_new(
                key,
                u64::try_from(stream_sequence).map_err(|_| {
                    StorageError::adapter("stored projection event sequence is negative")
                })?,
                Some(ControlPlaneEventId(event_id.to_owned())),
            )
            .map(Some)
        }
        _ => Err(StorageError::adapter(
            "stored projection event cursor columns are incomplete",
        )),
    }
}

fn stored_public_context(
    cursor: Option<&ProjectionEventCursor>,
    scope: Option<Vec<u8>>,
    stream: Option<Vec<u8>>,
    occurred_at: Option<Vec<u8>>,
    source: Option<Vec<u8>>,
    receipt_scope_key: &ReceiptScopeKey,
    receipt_actor_key: Option<&ReceiptActorKey>,
) -> Result<Option<PublicProjectionEventContext>, StorageError> {
    match (cursor, scope, stream, occurred_at, source) {
        (None, None, None, None, None) => Ok(None),
        (Some(cursor), Some(scope), Some(stream), Some(occurred_at), Some(source)) => {
            let context = PublicProjectionEventContext {
                scope: serde_json::from_slice(&scope).map_err(|error| json_error(&error))?,
                stream: serde_json::from_slice(&stream).map_err(|error| json_error(&error))?,
                occurred_at: serde_json::from_slice(&occurred_at)
                    .map_err(|error| json_error(&error))?,
                source: serde_json::from_slice(&source).map_err(|error| json_error(&error))?,
            };
            if cursor.key().stream() != context.stream() {
                return Err(StorageError::adapter(
                    "stored public context stream differs from its projection cursor",
                ));
            }
            validate_public_event_time(context.occurred_at())?;
            validate_public_source(context.source())?;
            validate_public_stream_source(context.stream(), context.source())?;
            if &crate::receipt_scope_key(context.scope())? != receipt_scope_key {
                return Err(StorageError::adapter(
                    "stored public context scope differs from its receipt scope",
                ));
            }
            if let (PublicEventSource::ControlPlane { actor, .. }, Some(receipt_actor_key)) =
                (context.source(), receipt_actor_key)
                && &crate::receipt_actor_key(actor)? != receipt_actor_key
            {
                return Err(StorageError::adapter(
                    "stored public context actor differs from its receipt actor",
                ));
            }
            Ok(Some(context))
        }
        _ => Err(StorageError::adapter(
            "public outbox context is incomplete or attached to an internal event",
        )),
    }
}

fn load_aggregate_journal(
    connection: &Connection,
    key: &AggregateJournalKey,
) -> Result<Option<LoadedAggregateJournal>, StorageError> {
    let manifest = connection
        .query_row(
            "SELECT manifest FROM aggregate_journals \
             WHERE aggregate_type = ?1 AND aggregate_id = ?2",
            params![key.aggregate_type(), key.aggregate_id()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(sql_error)?;
    let Some(manifest) = manifest else {
        return Ok(None);
    };
    let mut statement = connection
        .prepare(
            "SELECT sequence, digest, payload FROM aggregate_journal_records \
             WHERE aggregate_type = ?1 AND aggregate_id = ?2 ORDER BY sequence",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map(params![key.aggregate_type(), key.aggregate_id()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(sql_error)?;
    let records = rows
        .map(|row| {
            let (sequence, digest, payload) = row.map_err(sql_error)?;
            Ok(AggregateJournalRecord {
                sequence: u64::try_from(sequence)
                    .map_err(|_| StorageError::adapter("journal sequence is negative"))?,
                digest,
                payload,
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    Ok(Some(LoadedAggregateJournal { manifest, records }))
}

fn validate_sha256_digest(digest: &Sha256Digest) -> Result<(), StorageError> {
    let Some(hex) = digest.0.strip_prefix("sha256:") else {
        return Err(StorageError::invalid(
            "command_digest must be a sha256 digest",
        ));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(StorageError::invalid(
            "command_digest must contain 64 lowercase hexadecimal digits",
        ));
    }
    Ok(())
}

fn canonical_control_plane_event_id(value: &str) -> bool {
    value.strip_prefix("evt_").is_some_and(|suffix| {
        (16..=128).contains(&suffix.len())
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    })
}

fn portable_event_key(value: &str) -> bool {
    value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'@' | b'-')
    })
}

fn sha256_digest(bytes: &[u8]) -> Sha256Digest {
    let digest = Sha256::digest(bytes);
    Sha256Digest(format!("sha256:{digest:x}"))
}

fn sql_error(error: rusqlite::Error) -> StorageError {
    let message = format!("SQLite operation failed: {error}");
    drop(error);
    StorageError::adapter(message)
}

#[cfg(test)]
mod sqlite_open_deadline_tests {
    use super::*;

    #[test]
    fn busy_handler_install_does_not_rebase_the_absolute_open_deadline() {
        let connection = Connection::open_in_memory().expect("open in-memory SQLite");
        let absolute_deadline = StdInstant::now() + SQLITE_OPEN_TIMEOUT;

        set_open_busy_deadline(&connection, absolute_deadline).expect("install open busy handler");

        assert_eq!(SQLITE_OPEN_BUSY_DEADLINE.get(), Some(absolute_deadline));
        connection
            .busy_timeout(SQLITE_BUSY_TIMEOUT)
            .expect("restore runtime busy handler");
        SQLITE_OPEN_BUSY_DEADLINE.set(None);
    }
}
