// SPDX-License-Identifier: Apache-2.0

//! Append-only Delivery records and the atomic persistence seam.
//!
//! [`DeliveryJournalPort`] is the only interface a `SQLite` or `PostgreSQL`
//! adapter must implement. It moves opaque, verified bytes and performs one
//! compare-and-publish operation; domain validation, replay, revision checks,
//! record encoding, and digest-chain recovery stay inside this module.

use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fmt,
    str::FromStr,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{DeliveryId, RequestId};

use crate::{
    application::{
        CoordinationError, CoordinationErrorCode,
        attention::ResolvedAttentionTransition,
        session_binding::{SessionBindingIdentity, accept_worker_session, report_codex_thread},
        solution_review::resolve_current_solution_review,
        stage::StageAdvanceResult,
        task_breakdown::{
            DeliveryTaskBreakdownApprovedEvent, TaskBreakdownPromotionTransition,
            prepare_task_breakdown_promotion, restore_task_breakdown_event,
        },
        verdict::ComputedVerdictTransition,
    },
    domain::{
        Delivery, DeliveryStatus, portable_identifier, request_identifier,
        rework::{ValidatedReworkHistoryFact, derive_validated_rework_history},
        safe_non_negative,
    },
};

pub const DELIVERY_STORE_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryMutationOperation {
    #[serde(rename = "delivery.created")]
    DeliveryCreated,
    #[serde(rename = "delivery.spec.updated")]
    DeliverySpecUpdated,
    #[serde(rename = "stage.started")]
    StageStarted,
    #[serde(rename = "session.bound")]
    SessionBound,
    #[serde(rename = "attention.resolved")]
    AttentionResolved,
    #[serde(rename = "verdict.submitted")]
    VerdictSubmitted,
    #[serde(rename = "rework.clarified")]
    ReworkClarified,
    #[serde(rename = "delivery.task_breakdown.approved")]
    TaskBreakdownApproved,
}

impl FromStr for DeliveryMutationOperation {
    type Err = DeliveryStoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "delivery.created" => Ok(Self::DeliveryCreated),
            "delivery.spec.updated" => Ok(Self::DeliverySpecUpdated),
            "stage.started" => Ok(Self::StageStarted),
            "session.bound" => Ok(Self::SessionBound),
            "attention.resolved" => Ok(Self::AttentionResolved),
            "verdict.submitted" => Ok(Self::VerdictSubmitted),
            "rework.clarified" => Ok(Self::ReworkClarified),
            "delivery.task_breakdown.approved" => Ok(Self::TaskBreakdownApproved),
            _ => Err(store_error(
                DeliveryStoreErrorCode::InvalidStoreOptions,
                "delivery mutation operation is unsupported",
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStoreErrorCode {
    InvalidStoreOptions,
    DeliveryAlreadyExists,
    DeliveryNotFound,
    StoreCorrupt,
    DeliveryIdMismatch,
    RevisionConflict,
    ReviewSetStale,
    RequestConflict,
    StoreIoError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryStoreError {
    code: DeliveryStoreErrorCode,
    message: String,
    expected_revision: Option<u64>,
    current_revision: Option<u64>,
}

impl DeliveryStoreError {
    pub fn code(&self) -> DeliveryStoreErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// Revision supplied by the rejected command, when the failure is a
    /// concurrency conflict.
    pub const fn expected_revision(&self) -> Option<u64> {
        self.expected_revision
    }

    /// Authoritative revision observed while rejecting a stale command.
    pub const fn current_revision(&self) -> Option<u64> {
        self.current_revision
    }
}

impl fmt::Display for DeliveryStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for DeliveryStoreError {}

fn store_error(code: DeliveryStoreErrorCode, message: impl Into<String>) -> DeliveryStoreError {
    DeliveryStoreError {
        code,
        message: message.into(),
        expected_revision: None,
        current_revision: None,
    }
}

fn revision_conflict(
    expected_revision: u64,
    current_revision: u64,
    message: impl Into<String>,
) -> DeliveryStoreError {
    DeliveryStoreError {
        code: DeliveryStoreErrorCode::RevisionConflict,
        message: message.into(),
        expected_revision: Some(expected_revision),
        current_revision: Some(current_revision),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryStoreManifest {
    pub schema_version: u8,
    pub delivery_id: DeliveryId,
    pub created_at_millis: u64,
    pub first_record_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeliveryStoreRecordPayload<'a> {
    schema_version: u8,
    delivery_id: &'a DeliveryId,
    sequence: &'a str,
    request_id: &'a RequestId,
    request_digest: &'a str,
    operation: DeliveryMutationOperation,
    previous_digest: Option<&'a str>,
    snapshot: &'a Delivery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryStoreRecord {
    pub schema_version: u8,
    pub delivery_id: DeliveryId,
    pub sequence: String,
    pub request_id: RequestId,
    pub request_digest: String,
    pub operation: DeliveryMutationOperation,
    #[serde(deserialize_with = "crate::domain::deserialize_required_option")]
    pub previous_digest: Option<String>,
    pub snapshot: Delivery,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDelivery {
    pub manifest: DeliveryStoreManifest,
    pub records: Vec<DeliveryStoreRecord>,
    pub snapshot: Delivery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryStoreMutationResult {
    pub snapshot: Delivery,
    pub replayed: bool,
    pub task_breakdown_event: Option<DeliveryTaskBreakdownApprovedEvent>,
}

#[derive(Debug, Clone)]
pub struct CreateDelivery {
    pub request_id: RequestId,
    pub request_digest: String,
    pub snapshot: Delivery,
}

#[derive(Debug, Clone)]
pub struct AppendDelivery {
    pub delivery_id: DeliveryId,
    pub request_id: RequestId,
    pub request_digest: String,
    pub operation: DeliveryMutationOperation,
    pub expected_revision: u64,
    pub snapshot: Delivery,
}

/// Specialized task-promotion append. The command contains only durable
/// request identity and the approved review digest; the Store rebuilds the
/// exact task graph from the current canonical Delivery.
#[derive(Debug, Clone)]
pub struct ApproveDeliveryTaskBreakdown {
    pub delivery_id: DeliveryId,
    pub request_id: RequestId,
    pub request_digest: String,
    pub expected_revision: u64,
    pub review_set_sha256: String,
}

/// Specialized verdict append that can be created only by the application
/// service from sealed candidate, verification, and Evidence facts.
#[derive(Debug, Clone)]
pub struct SubmitDeliveryVerdict {
    pub request_id: RequestId,
    pub request_digest: String,
    pub expected_revision: u64,
    pub transition: ComputedVerdictTransition,
}

/// Specialized stage-start append created only from [`crate::application::stage::advance`].
#[derive(Debug, Clone)]
pub struct StartDeliveryStage {
    pub request_id: RequestId,
    pub request_digest: String,
    pub expected_revision: u64,
    pub transition: StageAdvanceResult,
}

/// Specialized Attention append created only from
/// [`crate::application::attention::resolve_attention`].
#[derive(Debug, Clone)]
pub struct ResolveDeliveryAttention {
    pub request_id: RequestId,
    pub request_digest: String,
    pub expected_revision: u64,
    pub transition: ResolvedAttentionTransition,
}

/// Specialized bounded-rework clarification append created only from
/// [`crate::application::stage::advance_rework`].
#[derive(Debug, Clone)]
pub struct ClarifyDeliveryRework {
    pub request_id: RequestId,
    pub request_digest: String,
    pub expected_revision: u64,
    pub transition: StageAdvanceResult,
}

#[derive(Debug, Clone)]
pub enum DeliveryCommand {
    Create(CreateDelivery),
    #[cfg(any(test, feature = "test-support"))]
    SeedForTest(CreateDelivery),
    Append(AppendDelivery),
    StartStage(Box<StartDeliveryStage>),
    ResolveAttention(Box<ResolveDeliveryAttention>),
    SubmitVerdict(Box<SubmitDeliveryVerdict>),
    ClarifyRework(Box<ClarifyDeliveryRework>),
    ApproveTaskBreakdown(Box<ApproveDeliveryTaskBreakdown>),
}

#[derive(Debug, Clone)]
pub enum DeliveryQuery {
    Get(DeliveryId),
    GetRevision {
        delivery_id: DeliveryId,
        revision: u64,
    },
}

/// One-method write interface for the Control Plane application layer.
///
/// This aggregate journal resolves replay only inside one Delivery. The HTTP
/// command receipt resolves the canonical `actor + scope + requestId` identity
/// before commands reach this port.
pub trait DeliveryCommandPort: Send + Sync {
    /// Applies a validated create or append command.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryStoreError`] for invalid input, replay conflicts,
    /// stale revisions, corrupt records, or backend publication failures.
    fn execute(
        &self,
        command: DeliveryCommand,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError>;
}

/// One-method read interface for the Control Plane application layer.
pub trait DeliveryQueryPort: Send + Sync {
    /// Reconstructs and verifies one Delivery from its record chain.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryStoreError`] when the Delivery is absent, corrupt, or
    /// cannot be loaded from the backend.
    fn query(&self, query: DeliveryQuery) -> Result<Delivery, DeliveryStoreError>;
}

/// Sealed append-only history projection used by the rework application path.
///
/// The adapter verifies the complete Delivery journal and returns only the
/// derived fact. Callers cannot omit or rewrite prior verdict snapshots.
pub trait DeliveryReworkHistoryPort: Send + Sync {
    /// Derives the current rework history from one verified journal tail.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryStoreError`] when the supplied Delivery is not the
    /// exact current tail or its append-only history is corrupt or incomplete.
    fn validated_rework_history(
        &self,
        delivery: &Delivery,
    ) -> Result<ValidatedReworkHistoryFact, DeliveryStoreError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalEntryState {
    Published,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecordBytes {
    pub sequence: u64,
    pub state: JournalEntryState,
    /// Digest metadata used for opaque compare-and-publish and verified on recovery.
    pub digest: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedDeliveryJournal {
    pub manifest: Vec<u8>,
    pub records: Vec<JournalRecordBytes>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtomicPublication {
    Create {
        delivery_id: DeliveryId,
        manifest: Vec<u8>,
        first_record: JournalRecordBytes,
    },
    Append {
        delivery_id: DeliveryId,
        expected_tail_sequence: u64,
        expected_tail_digest: String,
        record: JournalRecordBytes,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalBackendErrorCode {
    AlreadyExists,
    NotFound,
    Conflict,
    Io,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalBackendError {
    pub code: JournalBackendErrorCode,
    pub message: String,
}

impl JournalBackendError {
    pub fn new(code: JournalBackendErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Backend seam for `SQLite` (local) and `PostgreSQL` (enterprise) adapters.
///
/// `publish` must atomically compare the expected tail and publish or stage the
/// new record in its current transaction. A long-lived adapter makes it
/// authoritative before returning. A transaction-scoped adapter makes it
/// authoritative with the containing state-and-outbox commit. A pending record
/// is never authoritative and may be returned by `load`; recovery ignores it.
pub trait DeliveryJournalPort: Send + Sync {
    /// Loads the opaque manifest and all published or pending record bytes.
    ///
    /// # Errors
    ///
    /// Returns [`JournalBackendError`] for backend read failures.
    fn load(
        &self,
        delivery_id: &DeliveryId,
    ) -> Result<Option<LoadedDeliveryJournal>, JournalBackendError>;

    /// Atomically creates a journal or compares and appends one record.
    ///
    /// # Errors
    ///
    /// Returns [`JournalBackendError`] for an existing Delivery, a changed
    /// tail, a missing journal, or backend write failure.
    fn publish(&self, publication: AtomicPublication) -> Result<(), JournalBackendError>;
}

/// Canonical codec used by migration tools and persistence diagnostics.
///
/// Storage adapters normally keep these bytes opaque. Exposing the codec here
/// gives one verified implementation for import/recovery without making a
/// database adapter understand domain JSON or digest rules.
pub struct DeliveryJournalCodec;

impl DeliveryJournalCodec {
    /// Encodes a canonical journal manifest.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryStoreError`] if JSON serialization fails.
    pub fn encode_manifest(
        manifest: &DeliveryStoreManifest,
    ) -> Result<Vec<u8>, DeliveryStoreError> {
        encode_manifest(manifest)
    }

    /// Strictly decodes and validates a journal manifest.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryStoreError`] when the bytes are malformed or use an
    /// unsupported store schema.
    pub fn decode_manifest(bytes: &[u8]) -> Result<DeliveryStoreManifest, DeliveryStoreError> {
        decode_manifest(bytes)
    }

    /// Encodes one canonical append-only record.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryStoreError`] if JSON serialization fails.
    pub fn encode_record(record: &DeliveryStoreRecord) -> Result<Vec<u8>, DeliveryStoreError> {
        encode_record(record)
    }

    /// Strictly decodes and verifies one append-only record.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryStoreError`] for malformed bytes, invalid domain
    /// facts, or a changed digest.
    pub fn decode_record(bytes: &[u8]) -> Result<DeliveryStoreRecord, DeliveryStoreError> {
        decode_record(bytes)
    }

    /// Reconstructs one Delivery and verifies every chain relationship.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryStoreError`] for a malformed manifest, a gap,
    /// duplicate request, broken digest chain, or mismatched snapshot.
    pub fn verify(
        delivery_id: &DeliveryId,
        journal: LoadedDeliveryJournal,
    ) -> Result<StoredDelivery, DeliveryStoreError> {
        verify_journal(delivery_id, journal)
    }
}

enum DeliveryJournalHandle<'journal> {
    Borrowed(&'journal dyn DeliveryJournalPort),
    Shared(Arc<dyn DeliveryJournalPort>),
}

pub struct DeliveryStore<'journal> {
    journal: DeliveryJournalHandle<'journal>,
}

impl DeliveryStore<'static> {
    /// Builds a long-lived command/query module from a shared journal adapter.
    pub fn new<J>(journal: Arc<J>) -> Self
    where
        J: DeliveryJournalPort + 'static,
    {
        Self {
            journal: DeliveryJournalHandle::Shared(journal),
        }
    }
}

impl<'journal> DeliveryStore<'journal> {
    /// Builds a transaction-scoped module over a borrowed journal adapter.
    ///
    /// Phase 2.1 can use this constructor inside a `ProductStateStorage`
    /// transaction, stage [`AtomicPublication`] together with its outbox event,
    /// and make both authoritative in the outer transaction commit.
    pub fn borrowed(journal: &'journal dyn DeliveryJournalPort) -> Self {
        Self {
            journal: DeliveryJournalHandle::Borrowed(journal),
        }
    }

    fn journal(&self) -> &dyn DeliveryJournalPort {
        match &self.journal {
            DeliveryJournalHandle::Borrowed(journal) => *journal,
            DeliveryJournalHandle::Shared(journal) => journal.as_ref(),
        }
    }

    fn create(
        &self,
        command: CreateDelivery,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError> {
        validate_request(&command.request_id, &command.request_digest)?;
        validate_initial_delivery(&command.snapshot)?;
        self.create_authorized(command)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn seed_for_test(
        &self,
        command: CreateDelivery,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError> {
        validate_request(&command.request_id, &command.request_digest)?;
        if command.snapshot.revision() != 1 {
            return Err(store_error(
                DeliveryStoreErrorCode::InvalidStoreOptions,
                "a seeded Delivery journal must start at revision 1",
            ));
        }
        self.create_authorized(command)
    }

    fn create_authorized(
        &self,
        command: CreateDelivery,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError> {
        let record = materialize_record(
            command.snapshot.id().clone(),
            1,
            command.request_id,
            command.request_digest,
            DeliveryMutationOperation::DeliveryCreated,
            None,
            command.snapshot,
        )?;
        let manifest = DeliveryStoreManifest {
            schema_version: DELIVERY_STORE_SCHEMA_VERSION,
            delivery_id: record.delivery_id.clone(),
            created_at_millis: record.snapshot.snapshot().created_at_millis,
            first_record_digest: record.digest.clone(),
        };
        let publication = AtomicPublication::Create {
            delivery_id: record.delivery_id.clone(),
            manifest: encode_manifest(&manifest)?,
            first_record: JournalRecordBytes {
                sequence: 1,
                state: JournalEntryState::Published,
                digest: record.digest.clone(),
                bytes: encode_record(&record)?,
            },
        };
        match self.journal().publish(publication) {
            Ok(()) => Ok(DeliveryStoreMutationResult {
                snapshot: record.snapshot,
                replayed: false,
                task_breakdown_event: None,
            }),
            Err(error) if error.code == JournalBackendErrorCode::AlreadyExists => {
                let stored = self.read(&record.delivery_id)?;
                let first = stored.records.first().ok_or_else(|| {
                    store_error(
                        DeliveryStoreErrorCode::StoreCorrupt,
                        "delivery has no verified first record",
                    )
                })?;
                if first.request_id != record.request_id {
                    return Err(map_backend_error(error));
                }
                if first.request_digest != record.request_digest
                    || first.operation != DeliveryMutationOperation::DeliveryCreated
                {
                    return Err(store_error(
                        DeliveryStoreErrorCode::RequestConflict,
                        format!(
                            "request {} was already used for another delivery mutation",
                            record.request_id.0
                        ),
                    ));
                }
                Ok(DeliveryStoreMutationResult {
                    snapshot: first.snapshot.clone(),
                    replayed: true,
                    task_breakdown_event: None,
                })
            }
            Err(error) => Err(map_backend_error(error)),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn append(
        &self,
        command: AppendDelivery,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError> {
        if matches!(
            command.operation,
            DeliveryMutationOperation::StageStarted
                | DeliveryMutationOperation::AttentionResolved
                | DeliveryMutationOperation::VerdictSubmitted
                | DeliveryMutationOperation::ReworkClarified
                | DeliveryMutationOperation::TaskBreakdownApproved
        ) {
            return Err(store_error(
                DeliveryStoreErrorCode::InvalidStoreOptions,
                "stage, Attention, Verdict, rework clarification, and task-breakdown mutations require their sealed specialized Delivery commands",
            ));
        }
        self.append_authorized(command, AppendAuthority::Generic)
    }

    fn start_stage(
        &self,
        command: StartDeliveryStage,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError> {
        let append = AppendDelivery {
            delivery_id: command.transition.delivery.id().clone(),
            request_id: command.request_id,
            request_digest: command.request_digest,
            operation: DeliveryMutationOperation::StageStarted,
            expected_revision: command.expected_revision,
            snapshot: command.transition.delivery.clone(),
        };
        self.append_authorized(append, AppendAuthority::Stage(&command.transition))
    }

    fn resolve_attention(
        &self,
        command: ResolveDeliveryAttention,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError> {
        let append = AppendDelivery {
            delivery_id: command.transition.delivery().id().clone(),
            request_id: command.request_id,
            request_digest: command.request_digest,
            operation: DeliveryMutationOperation::AttentionResolved,
            expected_revision: command.expected_revision,
            snapshot: command.transition.delivery().clone(),
        };
        self.append_authorized(append, AppendAuthority::Attention(&command.transition))
    }

    fn submit_verdict(
        &self,
        command: SubmitDeliveryVerdict,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError> {
        let delivery_id = command.transition.delivery().id().clone();
        let append = AppendDelivery {
            delivery_id,
            request_id: command.request_id,
            request_digest: command.request_digest,
            operation: DeliveryMutationOperation::VerdictSubmitted,
            expected_revision: command.expected_revision,
            snapshot: command.transition.delivery().clone(),
        };
        self.append_authorized(append, AppendAuthority::Verdict(&command.transition))
    }

    fn clarify_rework(
        &self,
        command: ClarifyDeliveryRework,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError> {
        let append = AppendDelivery {
            delivery_id: command.transition.delivery.id().clone(),
            request_id: command.request_id,
            request_digest: command.request_digest,
            operation: DeliveryMutationOperation::ReworkClarified,
            expected_revision: command.expected_revision,
            snapshot: command.transition.delivery.clone(),
        };
        self.append_authorized(
            append,
            AppendAuthority::ReworkClarification(&command.transition),
        )
    }

    fn approve_task_breakdown(
        &self,
        command: ApproveDeliveryTaskBreakdown,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError> {
        validate_request(&command.request_id, &command.request_digest)?;
        validate_digest(
            &command.review_set_sha256,
            DeliveryStoreErrorCode::InvalidStoreOptions,
            "reviewSetSha256",
        )?;
        let stored = self.read(&command.delivery_id)?;

        if let Some((index, prior)) = stored
            .records
            .iter()
            .enumerate()
            .find(|(_, record)| record.request_id == command.request_id)
        {
            let prior_expected_revision =
                prior.snapshot.revision().checked_sub(1).ok_or_else(|| {
                    store_error(
                        DeliveryStoreErrorCode::StoreCorrupt,
                        "task-breakdown record cannot have revision zero",
                    )
                })?;
            if prior.request_digest != command.request_digest
                || prior.operation != DeliveryMutationOperation::TaskBreakdownApproved
                || prior_expected_revision != command.expected_revision
            {
                return Err(store_error(
                    DeliveryStoreErrorCode::RequestConflict,
                    format!(
                        "request {} was already used for another delivery mutation",
                        command.request_id.0
                    ),
                ));
            }
            let source = index
                .checked_sub(1)
                .and_then(|source_index| {
                    stored
                        .records
                        .get(source_index)
                        .map(|record| &record.snapshot)
                })
                .ok_or_else(|| {
                    store_error(
                        DeliveryStoreErrorCode::StoreCorrupt,
                        "task-breakdown record has no source Delivery revision",
                    )
                })?;
            let event =
                restore_task_breakdown_event(source, &prior.snapshot, &command.review_set_sha256)
                    .map_err(|error| map_task_breakdown_error(&error))?;
            return Ok(DeliveryStoreMutationResult {
                snapshot: prior.snapshot.clone(),
                replayed: true,
                task_breakdown_event: Some(event),
            });
        }

        if command.expected_revision != stored.snapshot.revision() {
            return Err(revision_conflict(
                command.expected_revision,
                stored.snapshot.revision(),
                "delivery revision changed before task-breakdown promotion",
            ));
        }
        let review = resolve_current_solution_review(&stored.snapshot)
            .map_err(|error| {
                store_error(
                    DeliveryStoreErrorCode::InvalidStoreOptions,
                    error.to_string(),
                )
            })?
            .ok_or_else(|| {
                store_error(
                    DeliveryStoreErrorCode::InvalidStoreOptions,
                    "task-breakdown promotion requires a current solution review",
                )
            })?;
        if review.review_set_sha256() != command.review_set_sha256 {
            return Err(store_error(
                DeliveryStoreErrorCode::ReviewSetStale,
                "reviewSetSha256 no longer identifies the current solution review",
            ));
        }
        let approved = review.approved_task_promotion().ok_or_else(|| {
            store_error(
                DeliveryStoreErrorCode::InvalidStoreOptions,
                "task-breakdown promotion requires the current approved solution review",
            )
        })?;
        let transition = prepare_task_breakdown_promotion(&stored.snapshot, &approved)
            .map_err(|error| map_task_breakdown_error(&error))?;
        let append = AppendDelivery {
            delivery_id: command.delivery_id,
            request_id: command.request_id,
            request_digest: command.request_digest,
            operation: DeliveryMutationOperation::TaskBreakdownApproved,
            expected_revision: command.expected_revision,
            snapshot: transition.delivery().clone(),
        };
        self.append_authorized(append, AppendAuthority::TaskBreakdown(&transition))
    }

    #[allow(clippy::too_many_lines)]
    fn append_authorized(
        &self,
        command: AppendDelivery,
        authority: AppendAuthority<'_>,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError> {
        validate_request(&command.request_id, &command.request_digest)?;
        if command.operation == DeliveryMutationOperation::DeliveryCreated {
            return Err(store_error(
                DeliveryStoreErrorCode::InvalidStoreOptions,
                "delivery.created is reserved for the first record",
            ));
        }
        let stored = self.read(&command.delivery_id)?;
        if command.snapshot.id() != &command.delivery_id {
            return Err(store_error(
                DeliveryStoreErrorCode::DeliveryIdMismatch,
                "delivery mutation snapshot belongs to another delivery",
            ));
        }
        if let Some(prior) = stored
            .records
            .iter()
            .find(|record| record.request_id == command.request_id)
        {
            let prior_expected_revision =
                prior.snapshot.revision().checked_sub(1).ok_or_else(|| {
                    store_error(
                        DeliveryStoreErrorCode::StoreCorrupt,
                        "append record cannot have revision zero",
                    )
                })?;
            if prior.request_digest != command.request_digest
                || prior.operation != command.operation
                || prior_expected_revision != command.expected_revision
            {
                return Err(store_error(
                    DeliveryStoreErrorCode::RequestConflict,
                    format!(
                        "request {} was already used for another delivery mutation",
                        command.request_id.0
                    ),
                ));
            }
            return Ok(DeliveryStoreMutationResult {
                snapshot: prior.snapshot.clone(),
                replayed: true,
                task_breakdown_event: authority.task_breakdown_event(),
            });
        }
        if command.expected_revision != stored.snapshot.revision()
            || command.snapshot.revision() != stored.snapshot.revision() + 1
        {
            return Err(revision_conflict(
                command.expected_revision,
                stored.snapshot.revision(),
                "delivery revision changed before mutation",
            ));
        }
        match authority {
            AppendAuthority::Generic => {
                validate_generic_append_delta(
                    command.operation,
                    &stored.snapshot,
                    &command.snapshot,
                )?;
            }
            AppendAuthority::Stage(transition) => {
                validate_authorized_operation(
                    command.operation,
                    DeliveryMutationOperation::StageStarted,
                    "stage.started",
                )?;
                transition
                    .validate_start_source(&stored.snapshot)
                    .map_err(|error| map_transition_error(&error, &command, &stored.snapshot))?;
            }
            AppendAuthority::Attention(transition) => {
                validate_authorized_operation(
                    command.operation,
                    DeliveryMutationOperation::AttentionResolved,
                    "attention.resolved",
                )?;
                transition
                    .validate_source(&stored.snapshot)
                    .map_err(|error| map_transition_error(&error, &command, &stored.snapshot))?;
            }
            AppendAuthority::Verdict(transition) => {
                validate_authorized_operation(
                    command.operation,
                    DeliveryMutationOperation::VerdictSubmitted,
                    "verdict.submitted",
                )?;
                transition
                    .validate_source(&stored.snapshot)
                    .map_err(|error| map_transition_error(&error, &command, &stored.snapshot))?;
            }
            AppendAuthority::ReworkClarification(transition) => {
                validate_authorized_operation(
                    command.operation,
                    DeliveryMutationOperation::ReworkClarified,
                    "rework.clarified",
                )?;
                transition
                    .validate_rework_clarification_source(&stored.snapshot)
                    .map_err(|error| map_transition_error(&error, &command, &stored.snapshot))?;
            }
            AppendAuthority::TaskBreakdown(transition) => {
                validate_authorized_operation(
                    command.operation,
                    DeliveryMutationOperation::TaskBreakdownApproved,
                    "delivery.task_breakdown.approved",
                )?;
                transition
                    .validate_source(&stored.snapshot)
                    .map_err(|error| map_task_breakdown_error(&error))?;
                if transition.delivery() != &command.snapshot
                    || transition.review_set_sha256() != transition.event().review_set_sha256
                {
                    return Err(store_error(
                        DeliveryStoreErrorCode::InvalidStoreOptions,
                        "task-breakdown command does not match its sealed transition",
                    ));
                }
            }
        }
        let previous = stored.records.last().ok_or_else(|| {
            store_error(
                DeliveryStoreErrorCode::StoreCorrupt,
                "delivery has no verified tail record",
            )
        })?;
        let sequence = parse_sequence(&previous.sequence)? + 1;
        if command.snapshot.revision() != sequence {
            return Err(revision_conflict(
                command.expected_revision,
                stored.snapshot.revision(),
                "delivery revision and durable sequence diverged",
            ));
        }
        let record = materialize_record(
            command.delivery_id.clone(),
            sequence,
            command.request_id.clone(),
            command.request_digest.clone(),
            command.operation,
            Some(previous.digest.clone()),
            command.snapshot,
        )?;
        let publication = AtomicPublication::Append {
            delivery_id: command.delivery_id.clone(),
            expected_tail_sequence: sequence - 1,
            expected_tail_digest: previous.digest.clone(),
            record: JournalRecordBytes {
                sequence,
                state: JournalEntryState::Published,
                digest: record.digest.clone(),
                bytes: encode_record(&record)?,
            },
        };
        match self.journal().publish(publication) {
            Ok(()) => Ok(DeliveryStoreMutationResult {
                snapshot: record.snapshot,
                replayed: false,
                task_breakdown_event: authority.task_breakdown_event(),
            }),
            Err(error) if error.code == JournalBackendErrorCode::Conflict => {
                let raced = self.read(&command.delivery_id)?;
                if let Some(prior) = raced
                    .records
                    .iter()
                    .find(|entry| entry.request_id == command.request_id)
                    && prior.request_digest == command.request_digest
                    && prior.operation == command.operation
                {
                    return Ok(DeliveryStoreMutationResult {
                        snapshot: prior.snapshot.clone(),
                        replayed: true,
                        task_breakdown_event: authority.task_breakdown_event(),
                    });
                }
                Err(revision_conflict(
                    command.expected_revision,
                    raced.snapshot.revision(),
                    "another mutation published this delivery revision",
                ))
            }
            Err(error) => Err(map_backend_error(error)),
        }
    }

    fn read(&self, delivery_id: &DeliveryId) -> Result<StoredDelivery, DeliveryStoreError> {
        portable_identifier(&delivery_id.0, "deliveryId").map_err(|error| {
            store_error(
                DeliveryStoreErrorCode::InvalidStoreOptions,
                error.to_string(),
            )
        })?;
        let loaded = self
            .journal()
            .load(delivery_id)
            .map_err(map_backend_error)?
            .ok_or_else(|| {
                store_error(
                    DeliveryStoreErrorCode::DeliveryNotFound,
                    format!("Delivery {} was not found", delivery_id.0),
                )
            })?;
        verify_journal(delivery_id, loaded)
    }
}

#[derive(Clone, Copy)]
enum AppendAuthority<'transition> {
    Generic,
    Stage(&'transition StageAdvanceResult),
    Attention(&'transition ResolvedAttentionTransition),
    Verdict(&'transition ComputedVerdictTransition),
    ReworkClarification(&'transition StageAdvanceResult),
    TaskBreakdown(&'transition TaskBreakdownPromotionTransition),
}

impl AppendAuthority<'_> {
    fn task_breakdown_event(self) -> Option<DeliveryTaskBreakdownApprovedEvent> {
        match self {
            Self::TaskBreakdown(transition) => Some(transition.event().clone()),
            Self::Generic
            | Self::Stage(_)
            | Self::Attention(_)
            | Self::Verdict(_)
            | Self::ReworkClarification(_) => None,
        }
    }
}

fn validate_authorized_operation(
    actual: DeliveryMutationOperation,
    expected: DeliveryMutationOperation,
    name: &str,
) -> Result<(), DeliveryStoreError> {
    if actual == expected {
        Ok(())
    } else {
        Err(store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            format!("sealed {name} transition was used for another operation"),
        ))
    }
}

fn map_transition_error(
    error: &CoordinationError,
    command: &AppendDelivery,
    current: &Delivery,
) -> DeliveryStoreError {
    if error.code() == CoordinationErrorCode::RevisionConflict {
        revision_conflict(
            command.expected_revision,
            current.revision(),
            error.to_string(),
        )
    } else {
        store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            error.to_string(),
        )
    }
}

fn map_task_breakdown_error(
    error: &crate::application::task_breakdown::TaskBreakdownPromotionError,
) -> DeliveryStoreError {
    let code = error.code();
    store_error(
        DeliveryStoreErrorCode::InvalidStoreOptions,
        format!("{code:?}: {}", error.message()),
    )
}

fn validate_generic_append_delta(
    operation: DeliveryMutationOperation,
    before: &Delivery,
    after: &Delivery,
) -> Result<(), DeliveryStoreError> {
    match operation {
        DeliveryMutationOperation::DeliverySpecUpdated => validate_spec_update_delta(before, after),
        DeliveryMutationOperation::SessionBound => validate_session_binding_delta(before, after),
        DeliveryMutationOperation::DeliveryCreated
        | DeliveryMutationOperation::StageStarted
        | DeliveryMutationOperation::AttentionResolved
        | DeliveryMutationOperation::VerdictSubmitted
        | DeliveryMutationOperation::ReworkClarified
        | DeliveryMutationOperation::TaskBreakdownApproved => Err(store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            "this Delivery operation requires its dedicated application command",
        )),
    }
}

fn validate_initial_delivery(delivery: &Delivery) -> Result<(), DeliveryStoreError> {
    let snapshot = delivery.snapshot();
    if snapshot.revision != 1
        || snapshot.status != DeliveryStatus::Draft
        || !snapshot.tasks.is_empty()
        || !snapshot.stage_runs.is_empty()
        || !snapshot.session_bindings.is_empty()
        || !snapshot.attention_items.is_empty()
        || !snapshot.evidence.is_empty()
        || snapshot.verdict.is_some()
        || snapshot.updated_at_millis != snapshot.created_at_millis
    {
        return Err(store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            "delivery.created requires the canonical empty revision-1 Draft transition",
        ));
    }
    Ok(())
}

fn validate_spec_update_delta(
    before: &Delivery,
    after: &Delivery,
) -> Result<(), DeliveryStoreError> {
    let before = before.snapshot();
    let after = after.snapshot();
    let valid = matches!(
        before.status,
        DeliveryStatus::Draft | DeliveryStatus::Clarifying | DeliveryStatus::Ready
    ) && after.schema_version == before.schema_version
        && after.id == before.id
        && after.revision == before.revision.saturating_add(1)
        && after.status == DeliveryStatus::Ready
        && after.created_at_millis == before.created_at_millis
        && after.updated_at_millis >= before.updated_at_millis
        && after.spec.delivery_id == before.id
        && after.spec.id != before.spec.id
        && after.spec.revision == before.spec.revision.saturating_add(1)
        && after.spec.created_at_millis >= before.spec.created_at_millis
        && after.spec.created_at_millis <= after.updated_at_millis
        && after.spec.source_ref == before.spec.source_ref
        && after.spec.publication_target == before.spec.publication_target
        && after.tasks.is_empty()
        && after.stage_runs.is_empty()
        && after.session_bindings.is_empty()
        && after.attention_items.is_empty()
        && after.evidence.is_empty()
        && after.verdict.is_none();
    if valid {
        Ok(())
    } else {
        Err(store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            "delivery.spec.updated does not match the canonical Spec replacement delta",
        ))
    }
}

fn validate_session_binding_delta(
    before: &Delivery,
    after: &Delivery,
) -> Result<(), DeliveryStoreError> {
    if before.snapshot().session_bindings.len() != after.snapshot().session_bindings.len() {
        return Err(store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            "session.bound cannot add or remove the stage-owned SessionBinding",
        ));
    }
    let changed = before
        .snapshot()
        .session_bindings
        .iter()
        .zip(&after.snapshot().session_bindings)
        .enumerate()
        .filter(|(_, (prior, next))| prior != next)
        .collect::<Vec<_>>();
    let [(index, (prior, next))] = changed.as_slice() else {
        return Err(store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            "session.bound must change exactly one current SessionBinding",
        ));
    };
    if prior.id != next.id {
        return Err(store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            "session.bound cannot replace a SessionBinding identity",
        ));
    }
    let identity = SessionBindingIdentity {
        delivery_id: prior.delivery_id.clone(),
        delivery_task_id: prior.delivery_task_id.clone(),
        stage_run_id: prior.stage_run_id.clone(),
        product_session_id: prior.product_session_id.clone(),
        execution_job_id: prior.execution_job_id.clone(),
    };
    let expected = match (
        prior.worker_session_id.as_ref(),
        next.worker_session_id.as_ref(),
        prior.codex_thread_id.as_ref(),
        next.codex_thread_id.as_ref(),
    ) {
        (None, Some(worker_session_id), prior_thread, next_thread)
            if prior_thread == next_thread =>
        {
            accept_worker_session(
                before,
                before.revision(),
                &identity,
                worker_session_id.clone(),
                after.snapshot().updated_at_millis,
            )
        }
        (Some(worker_session_id), Some(next_worker), None, Some(codex_thread_id))
            if worker_session_id == next_worker =>
        {
            report_codex_thread(
                before,
                before.revision(),
                &identity,
                worker_session_id,
                codex_thread_id.clone(),
                after.snapshot().updated_at_millis,
            )
        }
        _ => Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "session.bound changed an unsupported SessionBinding field",
        )),
    }
    .map_err(|error| {
        store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            error.to_string(),
        )
    })?;
    if expected != *after
        || expected.snapshot().session_bindings.get(*index)
            != after.snapshot().session_bindings.get(*index)
    {
        return Err(store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            "session.bound changed facts outside its exact application delta",
        ));
    }
    Ok(())
}

impl DeliveryCommandPort for DeliveryStore<'_> {
    fn execute(
        &self,
        command: DeliveryCommand,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError> {
        match command {
            DeliveryCommand::Create(create) => self.create(create),
            #[cfg(any(test, feature = "test-support"))]
            DeliveryCommand::SeedForTest(seed) => self.seed_for_test(seed),
            DeliveryCommand::Append(append) => self.append(append),
            DeliveryCommand::StartStage(start) => self.start_stage(*start),
            DeliveryCommand::ResolveAttention(resolve) => self.resolve_attention(*resolve),
            DeliveryCommand::SubmitVerdict(submit) => self.submit_verdict(*submit),
            DeliveryCommand::ClarifyRework(clarify) => self.clarify_rework(*clarify),
            DeliveryCommand::ApproveTaskBreakdown(approve) => self.approve_task_breakdown(*approve),
        }
    }
}

impl DeliveryQueryPort for DeliveryStore<'_> {
    fn query(&self, query: DeliveryQuery) -> Result<Delivery, DeliveryStoreError> {
        match query {
            DeliveryQuery::Get(delivery_id) => {
                self.read(&delivery_id).map(|stored| stored.snapshot)
            }
            DeliveryQuery::GetRevision {
                delivery_id,
                revision,
            } => self.read(&delivery_id).and_then(|stored| {
                stored
                    .records
                    .into_iter()
                    .find(|record| record.snapshot.revision() == revision)
                    .map(|record| record.snapshot)
                    .ok_or_else(|| {
                        store_error(
                            DeliveryStoreErrorCode::DeliveryNotFound,
                            format!(
                                "Delivery {} revision {revision} was not found",
                                delivery_id.0
                            ),
                        )
                    })
            }),
        }
    }
}

impl DeliveryReworkHistoryPort for DeliveryStore<'_> {
    fn validated_rework_history(
        &self,
        delivery: &Delivery,
    ) -> Result<ValidatedReworkHistoryFact, DeliveryStoreError> {
        let stored = self.read(delivery.id())?;
        if stored.snapshot != *delivery {
            return Err(revision_conflict(
                delivery.revision(),
                stored.snapshot.revision(),
                "rework history requires the exact current Delivery journal tail",
            ));
        }
        let history = stored
            .records
            .iter()
            .filter(|record| record.snapshot.revision() < delivery.revision())
            .map(|record| record.snapshot.snapshot().clone())
            .collect::<Vec<_>>();
        derive_validated_rework_history(delivery, &history).map_err(|error| {
            store_error(
                DeliveryStoreErrorCode::StoreCorrupt,
                format!("Delivery rework history is invalid: {error}"),
            )
        })
    }
}

fn validate_request(request_id: &RequestId, digest: &str) -> Result<(), DeliveryStoreError> {
    request_identifier(&request_id.0, "requestId").map_err(|error| {
        store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            error.to_string(),
        )
    })?;
    validate_digest(
        digest,
        DeliveryStoreErrorCode::InvalidStoreOptions,
        "requestDigest",
    )
}

fn validate_digest(
    digest: &str,
    code: DeliveryStoreErrorCode,
    label: &str,
) -> Result<(), DeliveryStoreError> {
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(store_error(code, format!("{label} is invalid")))
    }
}

fn record_payload(record: &DeliveryStoreRecord) -> Result<Vec<u8>, DeliveryStoreError> {
    serde_json::to_vec(&DeliveryStoreRecordPayload {
        schema_version: record.schema_version,
        delivery_id: &record.delivery_id,
        sequence: &record.sequence,
        request_id: &record.request_id,
        request_digest: &record.request_digest,
        operation: record.operation,
        previous_digest: record.previous_digest.as_deref(),
        snapshot: &record.snapshot,
    })
    .map_err(|error| store_error(DeliveryStoreErrorCode::StoreIoError, error.to_string()))
}

fn record_digest(record: &DeliveryStoreRecord) -> Result<String, DeliveryStoreError> {
    let digest = Sha256::digest(record_payload(record)?);
    Ok(format!("{digest:x}"))
}

fn materialize_record(
    delivery_id: DeliveryId,
    sequence: u64,
    request_id: RequestId,
    request_digest: String,
    operation: DeliveryMutationOperation,
    previous_digest: Option<String>,
    snapshot: Delivery,
) -> Result<DeliveryStoreRecord, DeliveryStoreError> {
    let mut record = DeliveryStoreRecord {
        schema_version: DELIVERY_STORE_SCHEMA_VERSION,
        delivery_id,
        sequence: sequence.to_string(),
        request_id,
        request_digest,
        operation,
        previous_digest,
        snapshot,
        digest: String::new(),
    };
    record.digest = record_digest(&record)?;
    Ok(record)
}

fn encode_manifest(manifest: &DeliveryStoreManifest) -> Result<Vec<u8>, DeliveryStoreError> {
    serde_json::to_vec(manifest)
        .map_err(|error| store_error(DeliveryStoreErrorCode::StoreIoError, error.to_string()))
}

fn encode_record(record: &DeliveryStoreRecord) -> Result<Vec<u8>, DeliveryStoreError> {
    serde_json::to_vec(record)
        .map_err(|error| store_error(DeliveryStoreErrorCode::StoreIoError, error.to_string()))
}

fn decode_manifest(bytes: &[u8]) -> Result<DeliveryStoreManifest, DeliveryStoreError> {
    let manifest: DeliveryStoreManifest = serde_json::from_slice(bytes).map_err(|error| {
        store_error(
            DeliveryStoreErrorCode::StoreCorrupt,
            format!("delivery manifest is corrupt: {error}"),
        )
    })?;
    if manifest.schema_version != DELIVERY_STORE_SCHEMA_VERSION {
        return Err(store_error(
            DeliveryStoreErrorCode::StoreCorrupt,
            "delivery manifest schemaVersion is unsupported",
        ));
    }
    portable_identifier(&manifest.delivery_id.0, "manifest deliveryId")
        .and_then(|()| safe_non_negative(manifest.created_at_millis, "manifest createdAtMillis"))
        .map_err(|error| store_error(DeliveryStoreErrorCode::StoreCorrupt, error.to_string()))?;
    validate_digest(
        &manifest.first_record_digest,
        DeliveryStoreErrorCode::StoreCorrupt,
        "manifest firstRecordDigest",
    )?;
    Ok(manifest)
}

fn decode_record(bytes: &[u8]) -> Result<DeliveryStoreRecord, DeliveryStoreError> {
    let record: DeliveryStoreRecord = serde_json::from_slice(bytes).map_err(|error| {
        store_error(
            DeliveryStoreErrorCode::StoreCorrupt,
            format!("delivery record is corrupt: {error}"),
        )
    })?;
    if record.schema_version != DELIVERY_STORE_SCHEMA_VERSION {
        return Err(store_error(
            DeliveryStoreErrorCode::StoreCorrupt,
            "delivery record schemaVersion is unsupported",
        ));
    }
    portable_identifier(&record.delivery_id.0, "record deliveryId")
        .map_err(|error| store_error(DeliveryStoreErrorCode::StoreCorrupt, error.to_string()))?;
    let sequence = parse_sequence(&record.sequence)?;
    validate_request(&record.request_id, &record.request_digest)
        .map_err(|error| store_error(DeliveryStoreErrorCode::StoreCorrupt, error.message))?;
    if let Some(previous) = &record.previous_digest {
        validate_digest(
            previous,
            DeliveryStoreErrorCode::StoreCorrupt,
            "record previousDigest",
        )?;
    }
    validate_digest(
        &record.digest,
        DeliveryStoreErrorCode::StoreCorrupt,
        "record digest",
    )?;
    if record_digest(&record)? != record.digest {
        return Err(store_error(
            DeliveryStoreErrorCode::StoreCorrupt,
            "delivery record digest changed",
        ));
    }
    if record.snapshot.id() != &record.delivery_id || record.snapshot.revision() != sequence {
        return Err(store_error(
            DeliveryStoreErrorCode::StoreCorrupt,
            "delivery record does not match its snapshot",
        ));
    }
    Ok(record)
}

fn parse_sequence(sequence: &str) -> Result<u64, DeliveryStoreError> {
    if sequence.starts_with('0') {
        return Err(store_error(
            DeliveryStoreErrorCode::StoreCorrupt,
            "delivery record sequence is invalid",
        ));
    }
    sequence
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            store_error(
                DeliveryStoreErrorCode::StoreCorrupt,
                "delivery record sequence is invalid",
            )
        })
}

fn verify_journal(
    delivery_id: &DeliveryId,
    loaded: LoadedDeliveryJournal,
) -> Result<StoredDelivery, DeliveryStoreError> {
    let manifest = decode_manifest(&loaded.manifest)?;
    if &manifest.delivery_id != delivery_id {
        return Err(store_error(
            DeliveryStoreErrorCode::StoreCorrupt,
            "delivery manifest identity changed",
        ));
    }
    let mut entries: Vec<_> = loaded
        .records
        .into_iter()
        .filter(|entry| entry.state == JournalEntryState::Published)
        .collect();
    entries.sort_by_key(|entry| entry.sequence);
    if entries.is_empty() {
        return Err(store_error(
            DeliveryStoreErrorCode::StoreCorrupt,
            "delivery has no records",
        ));
    }

    let mut records = Vec::with_capacity(entries.len());
    let mut previous_digest: Option<String> = None;
    let mut requests = HashSet::new();
    for (expected_sequence, entry) in (1_u64..).zip(entries) {
        if entry.sequence != expected_sequence {
            return Err(store_error(
                DeliveryStoreErrorCode::StoreCorrupt,
                "delivery record sequence is not contiguous",
            ));
        }
        let record = decode_record(&entry.bytes)?;
        if record.delivery_id != manifest.delivery_id
            || parse_sequence(&record.sequence)? != entry.sequence
            || record.digest != entry.digest
            || record.previous_digest != previous_digest
            || record.snapshot.revision() != entry.sequence
            || record.snapshot.id() != &manifest.delivery_id
        {
            return Err(store_error(
                DeliveryStoreErrorCode::StoreCorrupt,
                "delivery record has a broken relationship",
            ));
        }
        if !requests.insert(record.request_id.0.clone()) {
            return Err(store_error(
                DeliveryStoreErrorCode::StoreCorrupt,
                "delivery request is duplicated",
            ));
        }
        previous_digest = Some(record.digest.clone());
        records.push(record);
    }
    let first = records.first().ok_or_else(|| {
        store_error(
            DeliveryStoreErrorCode::StoreCorrupt,
            "delivery has no verified first record",
        )
    })?;
    if first.operation != DeliveryMutationOperation::DeliveryCreated
        || first.digest != manifest.first_record_digest
        || first.snapshot.snapshot().created_at_millis != manifest.created_at_millis
    {
        return Err(store_error(
            DeliveryStoreErrorCode::StoreCorrupt,
            "delivery manifest does not identify its first record",
        ));
    }
    let snapshot = records
        .last()
        .ok_or_else(|| {
            store_error(
                DeliveryStoreErrorCode::StoreCorrupt,
                "delivery has no verified tail record",
            )
        })?
        .snapshot
        .clone();
    Ok(StoredDelivery {
        manifest,
        records,
        snapshot,
    })
}

fn map_backend_error(error: JournalBackendError) -> DeliveryStoreError {
    let code = match error.code {
        JournalBackendErrorCode::AlreadyExists => DeliveryStoreErrorCode::DeliveryAlreadyExists,
        JournalBackendErrorCode::NotFound => DeliveryStoreErrorCode::DeliveryNotFound,
        JournalBackendErrorCode::Conflict => DeliveryStoreErrorCode::RevisionConflict,
        JournalBackendErrorCode::Io => DeliveryStoreErrorCode::StoreIoError,
    };
    store_error(code, error.message)
}

#[derive(Default)]
pub struct InMemoryDeliveryJournal {
    journals: Mutex<BTreeMap<String, LoadedDeliveryJournal>>,
}

impl InMemoryDeliveryJournal {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DeliveryJournalPort for InMemoryDeliveryJournal {
    fn load(
        &self,
        delivery_id: &DeliveryId,
    ) -> Result<Option<LoadedDeliveryJournal>, JournalBackendError> {
        let journals = self.journals.lock().map_err(|_| {
            JournalBackendError::new(JournalBackendErrorCode::Io, "memory journal lock poisoned")
        })?;
        Ok(journals.get(&delivery_id.0).cloned())
    }

    fn publish(&self, publication: AtomicPublication) -> Result<(), JournalBackendError> {
        let mut journals = self.journals.lock().map_err(|_| {
            JournalBackendError::new(JournalBackendErrorCode::Io, "memory journal lock poisoned")
        })?;
        match publication {
            AtomicPublication::Create {
                delivery_id,
                manifest,
                first_record,
            } => {
                if journals.contains_key(&delivery_id.0) {
                    return Err(JournalBackendError::new(
                        JournalBackendErrorCode::AlreadyExists,
                        "delivery already exists",
                    ));
                }
                if first_record.state != JournalEntryState::Published || first_record.sequence != 1
                {
                    return Err(JournalBackendError::new(
                        JournalBackendErrorCode::Io,
                        "first publication is malformed",
                    ));
                }
                journals.insert(
                    delivery_id.0,
                    LoadedDeliveryJournal {
                        manifest,
                        records: vec![first_record],
                    },
                );
                Ok(())
            }
            AtomicPublication::Append {
                delivery_id,
                expected_tail_sequence,
                expected_tail_digest,
                record,
            } => {
                let journal = journals.get_mut(&delivery_id.0).ok_or_else(|| {
                    JournalBackendError::new(
                        JournalBackendErrorCode::NotFound,
                        "delivery was not found",
                    )
                })?;
                let tail = journal
                    .records
                    .iter()
                    .filter(|entry| entry.state == JournalEntryState::Published)
                    .max_by_key(|entry| entry.sequence)
                    .ok_or_else(|| {
                        JournalBackendError::new(
                            JournalBackendErrorCode::Io,
                            "delivery has no published record",
                        )
                    })?;
                if tail.sequence != expected_tail_sequence
                    || tail.digest != expected_tail_digest
                    || record.state != JournalEntryState::Published
                    || record.sequence != expected_tail_sequence + 1
                {
                    return Err(JournalBackendError::new(
                        JournalBackendErrorCode::Conflict,
                        "delivery tail changed",
                    ));
                }
                journal.records.push(record);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::attention::{
        AttentionDecision, ResolveAttentionInput, resolve_attention,
    };
    use crate::application::stage::{AdvanceStageInput, NewStageIdentities, advance};
    use crate::application::verdict::{
        SubmitVerdictFacts, compute_verdict_transition,
        test_support::{VerdictFixtureOutcome, verdict_fixture},
    };

    const REQUEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const REQUEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn snapshot(revision: u64, status: &str) -> Delivery {
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/delivery-store.json"))
                .expect("store fixture JSON");
        value["revision"] = revision.into();
        value["status"] = status.into();
        value["updatedAtMillis"] = if revision == 1 {
            1_800_000_000_000_u64.into()
        } else {
            (1_800_000_000_000_u64 + revision).into()
        };
        if revision > 1 {
            value["spec"]["id"] = format!("delivery-spec-v{revision}").into();
            value["spec"]["revision"] = revision.into();
            value["spec"]["createdAtMillis"] = (1_800_000_000_000_u64 + revision).into();
        }
        Delivery::decode_json(&serde_json::to_vec(&value).expect("store fixture bytes"))
            .expect("store fixture")
    }

    fn create_store() -> (Arc<InMemoryDeliveryJournal>, DeliveryStore<'static>) {
        let backend = Arc::new(InMemoryDeliveryJournal::new());
        let store = DeliveryStore::new(Arc::clone(&backend));
        store
            .execute(DeliveryCommand::Create(CreateDelivery {
                request_id: RequestId("create-delivery".into()),
                request_digest: REQUEST_A.into(),
                snapshot: snapshot(1, "draft"),
            }))
            .expect("create");
        (backend, store)
    }

    fn create_store_with_failed_verdict() -> (
        DeliveryStore<'static>,
        Delivery,
        crate::domain::FrozenDeliveryCandidate,
    ) {
        let fixture = verdict_fixture(
            &DeliveryId("dlv_01J00000000000000000000003".into()),
            VerdictFixtureOutcome::Fail,
        );
        let backend = Arc::new(InMemoryDeliveryJournal::new());
        let store = DeliveryStore::new(backend);
        store
            .execute(DeliveryCommand::SeedForTest(CreateDelivery {
                request_id: RequestId("create-delivery-verdict".into()),
                request_digest: REQUEST_A.into(),
                snapshot: fixture.delivery.clone(),
            }))
            .expect("create verifying Delivery");
        let transition = compute_verdict_transition(
            &fixture.delivery,
            SubmitVerdictFacts {
                expected_revision: fixture.delivery.revision(),
                candidate: &fixture.candidate,
                verification: &fixture.verification,
                evidence: &fixture.evidence,
                produced_at_millis: 1_800_000_000_100,
            },
        )
        .expect("computed failing verdict");
        let failed = store
            .execute(DeliveryCommand::SubmitVerdict(Box::new(
                SubmitDeliveryVerdict {
                    request_id: RequestId("submit-failed-verdict".into()),
                    request_digest: REQUEST_B.into(),
                    expected_revision: fixture.delivery.revision(),
                    transition,
                },
            )))
            .expect("submit failing verdict")
            .snapshot;
        (store, failed, fixture.candidate)
    }

    #[test]
    fn store_derives_rework_history_from_verified_append_only_records() {
        let (store, current, _) = create_store_with_failed_verdict();
        let stale = store
            .query(DeliveryQuery::GetRevision {
                delivery_id: current.id().clone(),
                revision: 1,
            })
            .expect("prior journal revision");

        store
            .validated_rework_history(&current)
            .expect("sealed rework history from verified journal");
        assert_eq!(
            store
                .validated_rework_history(&stale)
                .expect_err("stale current Delivery")
                .code(),
            DeliveryStoreErrorCode::RevisionConflict
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one scenario proves raw rejection, typed resolution, and typed rework persistence"
    )]
    fn attention_resolution_requires_its_sealed_operation_specific_command() {
        let (store, failed, candidate) = create_store_with_failed_verdict();
        let item = failed
            .snapshot()
            .attention_items
            .first()
            .expect("failed verdict Attention")
            .clone();
        let transition = resolve_attention(
            &failed,
            ResolveAttentionInput {
                expected_revision: failed.revision(),
                attention_item_id: item.id,
                stage_run_id: item.stage_run_id.expect("verification StageRun"),
                expected_context: item.context,
                actor: "delivery-reviewer".into(),
                decision: AttentionDecision::Resolved,
                resolution: "start the bounded remediation".into(),
                now_millis: 1_800_000_000_200,
            },
        )
        .expect("current verdict Attention resolves");

        let raw_error = store
            .execute(DeliveryCommand::Append(AppendDelivery {
                delivery_id: failed.id().clone(),
                request_id: RequestId("raw-attention-resolution".into()),
                request_digest: "d".repeat(64),
                operation: DeliveryMutationOperation::AttentionResolved,
                expected_revision: failed.revision(),
                snapshot: transition.delivery().clone(),
            }))
            .expect_err("raw Attention snapshot has no application authority");
        assert_eq!(
            raw_error.code(),
            DeliveryStoreErrorCode::InvalidStoreOptions
        );

        let resolved = store
            .execute(DeliveryCommand::ResolveAttention(Box::new(
                ResolveDeliveryAttention {
                    request_id: RequestId("sealed-attention-resolution".into()),
                    request_digest: "e".repeat(64),
                    expected_revision: failed.revision(),
                    transition,
                },
            )))
            .expect("sealed Attention resolution commits");
        assert_eq!(
            resolved.snapshot.snapshot().status,
            DeliveryStatus::Reworking
        );

        let history = store
            .validated_rework_history(&resolved.snapshot)
            .expect("append-only rework history");
        let authorization = crate::domain::rework::fixture_precise_rework_authorization(
            &resolved.snapshot,
            &candidate,
            &history,
            "b".repeat(64),
        );
        let stage = advance(
            &resolved.snapshot,
            AdvanceStageInput {
                expected_revision: resolved.snapshot.revision(),
                product_session_id: winwincode_domain::ProductSessionId(
                    "product-session-bounded-rework".into(),
                ),
                identities: NewStageIdentities {
                    stage_run_id: winwincode_domain::StageRunId("stage-run-bounded-rework".into()),
                    execution_job_id: winwincode_domain::ExecutionJobId(
                        "execution-job-bounded-rework".into(),
                    ),
                    session_binding_id: crate::domain::SessionBindingId(
                        "session-binding-bounded-rework".into(),
                    ),
                    attention_item_id: winwincode_domain::AttentionItemId(
                        "attention-unused-bounded-rework".into(),
                    ),
                },
                review: None,
                previous_outcome: None,
                current_lease: None,
                rework_authorization: Some(Box::new(authorization)),
                now_millis: resolved.snapshot.snapshot().updated_at_millis + 1,
            },
        )
        .expect("authorized rework stage");
        let reworking = store
            .execute(DeliveryCommand::StartStage(Box::new(StartDeliveryStage {
                request_id: RequestId("sealed-start-bounded-rework".into()),
                request_digest: "1".repeat(64),
                expected_revision: resolved.snapshot.revision(),
                transition: stage,
            })))
            .expect("typed rework stage persists")
            .snapshot;
        assert_eq!(
            reworking
                .snapshot()
                .stage_runs
                .last()
                .map(|run| run.role.as_str()),
            Some("remediator")
        );
        assert!(reworking.snapshot().evidence.is_empty());
        assert!(reworking.snapshot().verdict.is_none());
    }

    #[test]
    fn non_verdict_append_cannot_change_evidence_verdict_attention_or_status() {
        for operation in [
            DeliveryMutationOperation::DeliverySpecUpdated,
            DeliveryMutationOperation::StageStarted,
            DeliveryMutationOperation::SessionBound,
            DeliveryMutationOperation::AttentionResolved,
        ] {
            let (store, failed, _) = create_store_with_failed_verdict();
            let passing_fixture = verdict_fixture(
                &DeliveryId("dlv_01J00000000000000000000003".into()),
                VerdictFixtureOutcome::Pass,
            );
            let passing = compute_verdict_transition(
                &passing_fixture.delivery,
                SubmitVerdictFacts {
                    expected_revision: passing_fixture.delivery.revision(),
                    candidate: &passing_fixture.candidate,
                    verification: &passing_fixture.verification,
                    evidence: &passing_fixture.evidence,
                    produced_at_millis: 1_800_000_000_100,
                },
            )
            .expect("computed passing verdict");
            let mut forged = passing.delivery().snapshot().clone();
            forged.revision = failed.revision() + 1;
            forged.updated_at_millis += 1;
            let forged = Delivery::try_from_snapshot(forged).expect("valid forged snapshot");

            let error = store
                .execute(DeliveryCommand::Append(AppendDelivery {
                    delivery_id: failed.id().clone(),
                    request_id: RequestId(format!("raw-protected-{operation:?}")),
                    request_digest: "c".repeat(64),
                    operation,
                    expected_revision: failed.revision(),
                    snapshot: forged,
                }))
                .expect_err("non-verdict append cannot replace verdict facts");
            assert_eq!(error.code(), DeliveryStoreErrorCode::InvalidStoreOptions);
        }
    }

    #[test]
    fn creation_rejects_a_precomputed_passing_delivery() {
        let fixture = verdict_fixture(
            &DeliveryId("dlv_01J00000000000000000000004".into()),
            VerdictFixtureOutcome::Pass,
        );
        let transition = compute_verdict_transition(
            &fixture.delivery,
            SubmitVerdictFacts {
                expected_revision: fixture.delivery.revision(),
                candidate: &fixture.candidate,
                verification: &fixture.verification,
                evidence: &fixture.evidence,
                produced_at_millis: 1_800_000_000_100,
            },
        )
        .expect("computed pass");
        let mut forged = transition.delivery().snapshot().clone();
        forged.revision = 1;
        let forged = Delivery::try_from_snapshot(forged).expect("valid revision-1 pass");
        let store = DeliveryStore::new(Arc::new(InMemoryDeliveryJournal::new()));

        let error = store
            .execute(DeliveryCommand::Create(CreateDelivery {
                request_id: RequestId("raw-created-pass".into()),
                request_digest: REQUEST_A.into(),
                snapshot: forged,
            }))
            .expect_err("delivery.created cannot carry a precomputed pass");

        assert_eq!(error.code(), DeliveryStoreErrorCode::InvalidStoreOptions);
    }

    #[test]
    fn operation_relabel_cannot_resolve_verdict_attention_or_enter_rework() {
        for operation in [
            DeliveryMutationOperation::DeliveryCreated,
            DeliveryMutationOperation::DeliverySpecUpdated,
            DeliveryMutationOperation::StageStarted,
            DeliveryMutationOperation::SessionBound,
            DeliveryMutationOperation::AttentionResolved,
            DeliveryMutationOperation::VerdictSubmitted,
        ] {
            let (store, failed, _) = create_store_with_failed_verdict();
            let mut forged = failed.snapshot().clone();
            forged.revision += 1;
            forged.updated_at_millis += 1;
            forged.status = DeliveryStatus::Reworking;
            forged.tasks[0].status = crate::domain::DeliveryTaskStatus::Active;
            let item = forged
                .attention_items
                .first_mut()
                .expect("verdict Attention");
            item.status = crate::domain::AttentionItemStatus::Resolved;
            item.resolution = Some("forged rework approval".into());
            item.resolved_by = Some("attacker".into());
            item.resolved_at_millis = Some(forged.updated_at_millis);
            let forged = Delivery::try_from_snapshot(forged).expect("valid forged rework state");

            let error = store
                .execute(DeliveryCommand::Append(AppendDelivery {
                    delivery_id: failed.id().clone(),
                    request_id: RequestId(format!("relabel-{operation:?}")),
                    request_digest: "f".repeat(64),
                    operation,
                    expected_revision: failed.revision(),
                    snapshot: forged,
                }))
                .expect_err("operation relabel cannot gain application authority");
            assert_eq!(error.code(), DeliveryStoreErrorCode::InvalidStoreOptions);
        }
    }

    #[test]
    fn request_id_replays_identical_mutation_and_rejects_conflict() {
        let (_, store) = create_store();
        let command = AppendDelivery {
            delivery_id: DeliveryId("dlv_01J00000000000000000000002".into()),
            request_id: RequestId("update-spec".into()),
            request_digest: REQUEST_B.into(),
            operation: DeliveryMutationOperation::DeliverySpecUpdated,
            expected_revision: 1,
            snapshot: snapshot(2, "ready"),
        };
        assert!(!store.append(command.clone()).expect("append").replayed);
        assert!(store.append(command.clone()).expect("replay").replayed);
        let mut conflicting = command;
        conflicting.request_digest = REQUEST_A.into();
        assert_eq!(
            store.append(conflicting).expect_err("conflict").code(),
            DeliveryStoreErrorCode::RequestConflict
        );
    }

    #[test]
    fn expected_revision_and_next_revision_must_match() {
        let (_, store) = create_store();
        let error = store
            .append(AppendDelivery {
                delivery_id: DeliveryId("dlv_01J00000000000000000000002".into()),
                request_id: RequestId("stale".into()),
                request_digest: REQUEST_B.into(),
                operation: DeliveryMutationOperation::DeliverySpecUpdated,
                expected_revision: 0,
                snapshot: snapshot(2, "ready"),
            })
            .expect_err("revision conflict");
        assert_eq!(error.code(), DeliveryStoreErrorCode::RevisionConflict);
        assert_eq!(error.expected_revision(), Some(0));
        assert_eq!(error.current_revision(), Some(1));
    }

    #[test]
    fn generic_append_rejects_raw_verdict_submitted_snapshot() {
        let (_, store) = create_store();
        let error = store
            .append(AppendDelivery {
                delivery_id: DeliveryId("dlv_01J00000000000000000000002".into()),
                request_id: RequestId("raw-verdict".into()),
                request_digest: REQUEST_B.into(),
                operation: DeliveryMutationOperation::VerdictSubmitted,
                expected_revision: 1,
                snapshot: snapshot(2, "ready"),
            })
            .expect_err("raw VerdictSubmitted append");

        assert_eq!(error.code(), DeliveryStoreErrorCode::InvalidStoreOptions);
    }

    #[test]
    fn delivery_records_form_contiguous_digest_chain() {
        let (_, store) = create_store();
        store
            .append(AppendDelivery {
                delivery_id: DeliveryId("dlv_01J00000000000000000000002".into()),
                request_id: RequestId("update".into()),
                request_digest: REQUEST_B.into(),
                operation: DeliveryMutationOperation::DeliverySpecUpdated,
                expected_revision: 1,
                snapshot: snapshot(2, "ready"),
            })
            .expect("append");
        let stored = store.read(snapshot(1, "draft").id()).expect("read");
        assert_eq!(
            stored.records[1].previous_digest.as_deref(),
            Some(stored.records[0].digest.as_str())
        );
    }

    #[test]
    fn delivery_record_digest_matches_canonical_fixture() {
        let (_, store) = create_store();
        store
            .append(AppendDelivery {
                delivery_id: DeliveryId("dlv_01J00000000000000000000002".into()),
                request_id: RequestId("update-spec".into()),
                request_digest: REQUEST_B.into(),
                operation: DeliveryMutationOperation::DeliverySpecUpdated,
                expected_revision: 1,
                snapshot: snapshot(2, "ready"),
            })
            .expect("append");
        let stored = store.read(snapshot(1, "draft").id()).expect("read");
        assert_eq!(
            stored
                .records
                .iter()
                .map(|record| record.digest.as_str())
                .collect::<Vec<_>>(),
            [
                "2e15941dc69500ae73cf21052ea3378c337599463a544d2ea162634b7c8368de",
                "ad36db8bd4f227c9a7d0fa24e70191b0f694b3f7f4af8ffa139d809b6d481f8e",
            ]
        );
    }

    #[test]
    fn corrupt_delivery_store_is_rejected() {
        let (backend, store) = create_store();
        let mut journals = backend.journals.lock().expect("lock");
        let journal = journals
            .get_mut("dlv_01J00000000000000000000002")
            .expect("journal");
        let mut value: serde_json::Value =
            serde_json::from_slice(&journal.records[0].bytes).expect("record json");
        value["snapshot"]["status"] = serde_json::Value::String("ready".into());
        journal.records[0].bytes = serde_json::to_vec(&value).expect("json");
        drop(journals);
        assert_eq!(
            store
                .read(snapshot(1, "draft").id())
                .expect_err("corrupt")
                .code(),
            DeliveryStoreErrorCode::StoreCorrupt
        );
    }

    #[test]
    fn journal_digest_metadata_must_match_opaque_record_bytes() {
        let (backend, store) = create_store();
        backend
            .journals
            .lock()
            .expect("lock")
            .get_mut("dlv_01J00000000000000000000002")
            .expect("journal")
            .records[0]
            .digest = REQUEST_B.into();

        assert_eq!(
            store
                .read(snapshot(1, "draft").id())
                .expect_err("digest metadata mismatch")
                .code(),
            DeliveryStoreErrorCode::StoreCorrupt
        );
    }

    #[test]
    fn codec_rejects_invalid_persisted_delivery_identities() {
        let (_, store) = create_store();
        let stored = store.read(snapshot(1, "draft").id()).expect("read");

        let mut manifest = stored.manifest;
        manifest.delivery_id = DeliveryId("delivery\ninvalid".into());
        assert_eq!(
            DeliveryJournalCodec::decode_manifest(
                &DeliveryJournalCodec::encode_manifest(&manifest).expect("manifest bytes")
            )
            .expect_err("invalid manifest identity")
            .code(),
            DeliveryStoreErrorCode::StoreCorrupt
        );

        let mut record = stored.records[0].clone();
        record.delivery_id = DeliveryId("delivery\ninvalid".into());
        record.digest = record_digest(&record).expect("record digest");
        assert_eq!(
            DeliveryJournalCodec::decode_record(
                &DeliveryJournalCodec::encode_record(&record).expect("record bytes")
            )
            .expect_err("invalid record identity")
            .code(),
            DeliveryStoreErrorCode::StoreCorrupt
        );
    }

    #[test]
    fn concurrent_append_publishes_one_revision() {
        let (backend, _) = create_store();
        let left = DeliveryStore::new(Arc::clone(&backend));
        let right = DeliveryStore::new(Arc::clone(&backend));
        let left = std::thread::spawn(move || {
            left.append(AppendDelivery {
                delivery_id: DeliveryId("dlv_01J00000000000000000000002".into()),
                request_id: RequestId("left".into()),
                request_digest: REQUEST_A.into(),
                operation: DeliveryMutationOperation::DeliverySpecUpdated,
                expected_revision: 1,
                snapshot: snapshot(2, "ready"),
            })
        });
        let right = std::thread::spawn(move || {
            right.append(AppendDelivery {
                delivery_id: DeliveryId("dlv_01J00000000000000000000002".into()),
                request_id: RequestId("right".into()),
                request_digest: REQUEST_B.into(),
                operation: DeliveryMutationOperation::DeliverySpecUpdated,
                expected_revision: 1,
                snapshot: snapshot(2, "ready"),
            })
        });
        let results = [
            left.join().expect("left thread"),
            right.join().expect("right thread"),
        ];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let loser = results
            .iter()
            .find_map(|result| result.as_ref().err())
            .expect("one concurrent writer loses");
        assert_eq!(loser.expected_revision(), Some(1));
        assert_eq!(loser.current_revision(), Some(2));
        let reader = DeliveryStore::new(backend);
        assert_eq!(
            reader
                .read(snapshot(1, "draft").id())
                .expect("read")
                .records
                .len(),
            2
        );
    }

    #[test]
    fn pending_record_is_ignored_during_recovery() {
        let (backend, store) = create_store();
        backend
            .journals
            .lock()
            .expect("lock")
            .get_mut("dlv_01J00000000000000000000002")
            .expect("journal")
            .records
            .push(JournalRecordBytes {
                sequence: 2,
                state: JournalEntryState::Pending,
                digest: REQUEST_B.into(),
                bytes: b"not authoritative".to_vec(),
            });
        assert_eq!(
            store
                .read(snapshot(1, "draft").id())
                .expect("read")
                .records
                .len(),
            1
        );
    }

    #[test]
    fn borrowed_journal_supports_transaction_scoped_storage_adapter() {
        let backend = InMemoryDeliveryJournal::new();
        let store = DeliveryStore::borrowed(&backend);
        let result = store
            .execute(DeliveryCommand::Create(CreateDelivery {
                request_id: RequestId("create-delivery".into()),
                request_digest: REQUEST_A.into(),
                snapshot: snapshot(1, "draft"),
            }))
            .expect("transaction-scoped create");
        assert_eq!(result.snapshot.revision(), 1);
    }
}
