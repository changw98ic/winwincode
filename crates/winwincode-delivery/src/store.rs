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
    application::{CoordinationErrorCode, verdict::ComputedVerdictTransition},
    domain::{Delivery, portable_identifier, request_identifier, safe_non_negative},
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

/// Specialized verdict append that can be created only by the application
/// service from sealed candidate, verification, and Evidence facts.
#[derive(Debug, Clone)]
pub struct SubmitDeliveryVerdict {
    pub request_id: RequestId,
    pub request_digest: String,
    pub expected_revision: u64,
    pub transition: ComputedVerdictTransition,
}

#[derive(Debug, Clone)]
pub enum DeliveryCommand {
    Create(CreateDelivery),
    Append(AppendDelivery),
    SubmitVerdict(SubmitDeliveryVerdict),
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
        if command.snapshot.revision() != 1 {
            return Err(store_error(
                DeliveryStoreErrorCode::InvalidStoreOptions,
                "a new Delivery must start at revision 1",
            ));
        }
        if command.snapshot.snapshot().verdict.is_some()
            || !command.snapshot.snapshot().evidence.is_empty()
            || command
                .snapshot
                .snapshot()
                .attention_items
                .iter()
                .any(is_verdict_attention)
        {
            return Err(store_error(
                DeliveryStoreErrorCode::InvalidStoreOptions,
                "Delivery creation cannot seed computed Evidence, Verdict, or verdict Attention",
            ));
        }
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
        if command.operation == DeliveryMutationOperation::VerdictSubmitted {
            return Err(store_error(
                DeliveryStoreErrorCode::InvalidStoreOptions,
                "verdict.submitted requires the sealed specialized Delivery command",
            ));
        }
        self.append_authorized(command, None)
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
        self.append_authorized(append, Some(&command.transition))
    }

    #[allow(clippy::too_many_lines)]
    fn append_authorized(
        &self,
        command: AppendDelivery,
        verdict_transition: Option<&ComputedVerdictTransition>,
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
            });
        }
        if let Some(transition) = verdict_transition {
            transition
                .validate_source(&stored.snapshot)
                .map_err(|error| {
                    if error.code() == CoordinationErrorCode::RevisionConflict {
                        revision_conflict(
                            command.expected_revision,
                            stored.snapshot.revision(),
                            error.to_string(),
                        )
                    } else {
                        store_error(
                            DeliveryStoreErrorCode::InvalidStoreOptions,
                            error.to_string(),
                        )
                    }
                })?;
        } else if command.operation == DeliveryMutationOperation::VerdictSubmitted {
            return Err(store_error(
                DeliveryStoreErrorCode::InvalidStoreOptions,
                "verdict.submitted is missing its sealed computed transition",
            ));
        } else {
            validate_generic_verdict_delta(command.operation, &stored.snapshot, &command.snapshot)?;
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

fn validate_generic_verdict_delta(
    operation: DeliveryMutationOperation,
    before: &Delivery,
    after: &Delivery,
) -> Result<(), DeliveryStoreError> {
    let before = before.snapshot();
    let after = after.snapshot();
    let preserved_verdict = after.verdict == before.verdict;
    let cleared_verdict = before.verdict.is_some() && after.verdict.is_none();
    if !preserved_verdict && !cleared_verdict {
        return Err(store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            "only the sealed verdict command may add or replace a Delivery verdict",
        ));
    }

    if preserved_verdict && after.evidence != before.evidence {
        return Err(store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            "a generic Delivery operation cannot add, replace, or partially remove computed Evidence",
        ));
    }
    if cleared_verdict
        && (!after.evidence.is_empty()
            || !verdict_invalidation_is_authorized(operation, before, after))
    {
        return Err(store_error(
            DeliveryStoreErrorCode::InvalidStoreOptions,
            "computed Evidence and Verdict may be cleared only by a new Spec or an exact remediator writer start",
        ));
    }

    for item in &after.attention_items {
        match before
            .attention_items
            .iter()
            .find(|stored| stored.id == item.id)
        {
            None if is_verdict_attention(item) => {
                return Err(store_error(
                    DeliveryStoreErrorCode::InvalidStoreOptions,
                    "only the sealed verdict command may add verdict-derived Attention",
                ));
            }
            Some(stored)
                if stored.context != item.context
                    && (is_verdict_attention(stored) || is_verdict_attention(item)) =>
            {
                return Err(store_error(
                    DeliveryStoreErrorCode::InvalidStoreOptions,
                    "a generic Delivery operation cannot replace verdict Attention context",
                ));
            }
            None | Some(_) => {}
        }
    }
    Ok(())
}

fn verdict_invalidation_is_authorized(
    operation: DeliveryMutationOperation,
    before: &crate::domain::DeliverySnapshot,
    after: &crate::domain::DeliverySnapshot,
) -> bool {
    match operation {
        DeliveryMutationOperation::DeliverySpecUpdated => {
            after.spec != before.spec && after.spec.revision > before.spec.revision
        }
        DeliveryMutationOperation::StageStarted => {
            let new_runs = after
                .stage_runs
                .iter()
                .filter(|run| !before.stage_runs.iter().any(|stored| stored.id == run.id))
                .collect::<Vec<_>>();
            let [run] = new_runs.as_slice() else {
                return false;
            };
            run.stage == crate::domain::DeliveryStage::Reworking
                && run.actor_type == crate::domain::StageRunActorType::Codex
                && run.role == "remediator"
                && run.status == crate::domain::StageRunStatus::Running
                && after.status == crate::domain::DeliveryStatus::Reworking
                && after
                    .session_bindings
                    .iter()
                    .any(|binding| binding.stage_run_id == run.id)
        }
        DeliveryMutationOperation::DeliveryCreated
        | DeliveryMutationOperation::SessionBound
        | DeliveryMutationOperation::AttentionResolved
        | DeliveryMutationOperation::VerdictSubmitted => false,
    }
}

fn is_verdict_attention(item: &crate::domain::AttentionItem) -> bool {
    serde_json::from_str::<serde_json::Value>(&item.context)
        .ok()
        .and_then(|context| {
            context
                .get("protocol")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("winwincode.delivery-verdict-attention.v1")
}

impl DeliveryCommandPort for DeliveryStore<'_> {
    fn execute(
        &self,
        command: DeliveryCommand,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError> {
        match command {
            DeliveryCommand::Create(create) => self.create(create),
            DeliveryCommand::Append(append) => self.append(append),
            DeliveryCommand::SubmitVerdict(submit) => self.submit_verdict(submit),
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
        value["updatedAtMillis"] = (1_800_000_000_000_u64 + revision).into();
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

    #[test]
    fn request_id_replays_identical_mutation_and_rejects_conflict() {
        let (_, store) = create_store();
        let command = AppendDelivery {
            delivery_id: DeliveryId("delivery-store-main".into()),
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
                delivery_id: DeliveryId("delivery-store-main".into()),
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
                delivery_id: DeliveryId("delivery-store-main".into()),
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
    fn every_generic_append_operation_rejects_a_fabricated_passing_verdict() {
        for (index, operation) in [
            DeliveryMutationOperation::DeliverySpecUpdated,
            DeliveryMutationOperation::StageStarted,
            DeliveryMutationOperation::SessionBound,
            DeliveryMutationOperation::AttentionResolved,
        ]
        .into_iter()
        .enumerate()
        {
            let fixture = verdict_fixture(
                DeliveryId(format!("delivery-verdict-bypass-{index}")),
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
            .expect("sealed passing transition fixture");
            let backend = Arc::new(InMemoryDeliveryJournal::new());
            let store = DeliveryStore::new(Arc::clone(&backend));
            store
                .execute(DeliveryCommand::Create(CreateDelivery {
                    request_id: RequestId(format!("create-verdict-bypass-{index}")),
                    request_digest: REQUEST_A.into(),
                    snapshot: fixture.delivery.clone(),
                }))
                .expect("create verifying Delivery");

            let error = store
                .execute(DeliveryCommand::Append(AppendDelivery {
                    delivery_id: fixture.delivery.id().clone(),
                    request_id: RequestId(format!("raw-pass-{index}")),
                    request_digest: REQUEST_B.into(),
                    operation,
                    expected_revision: fixture.delivery.revision(),
                    snapshot: transition.delivery().clone(),
                }))
                .expect_err("generic operation must not submit a passing verdict");

            assert_eq!(error.code(), DeliveryStoreErrorCode::InvalidStoreOptions);
        }
    }

    #[test]
    fn delivery_records_form_contiguous_digest_chain() {
        let (_, store) = create_store();
        store
            .append(AppendDelivery {
                delivery_id: DeliveryId("delivery-store-main".into()),
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
    fn delivery_record_digest_matches_typescript_fixture() {
        let (_, store) = create_store();
        store
            .append(AppendDelivery {
                delivery_id: DeliveryId("delivery-store-main".into()),
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
                "59f1408d8139a10759060bdf3fe30c938448b5cd37d08fc9e70ee3897c0d02fa",
                "54434ed54bf23eda55e7fcf1704a16ed6373bd0746f858a6070d123481f89852",
            ]
        );
    }

    #[test]
    fn corrupt_delivery_store_is_rejected() {
        let (backend, store) = create_store();
        let mut journals = backend.journals.lock().expect("lock");
        let journal = journals.get_mut("delivery-store-main").expect("journal");
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
            .get_mut("delivery-store-main")
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
                delivery_id: DeliveryId("delivery-store-main".into()),
                request_id: RequestId("left".into()),
                request_digest: REQUEST_A.into(),
                operation: DeliveryMutationOperation::DeliverySpecUpdated,
                expected_revision: 1,
                snapshot: snapshot(2, "ready"),
            })
        });
        let right = std::thread::spawn(move || {
            right.append(AppendDelivery {
                delivery_id: DeliveryId("delivery-store-main".into()),
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
            .get_mut("delivery-store-main")
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
