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
use winwincode_api::generated::{DeliveryId, RequestId};

use crate::domain::{Delivery, request_identifier};

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
}

impl DeliveryStoreError {
    pub fn code(&self) -> DeliveryStoreErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
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

#[derive(Debug, Clone)]
pub enum DeliveryCommand {
    Create(CreateDelivery),
    Append(AppendDelivery),
}

#[derive(Debug, Clone)]
pub enum DeliveryQuery {
    Get(DeliveryId),
}

/// One-method write interface for the Control Plane application layer.
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
    fn query(&self, query: DeliveryQuery) -> Result<StoredDelivery, DeliveryStoreError>;
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
/// `publish` must atomically compare the expected tail and make the new
/// record authoritative. A pending record is never authoritative and may be
/// returned by `load`; recovery ignores it.
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

pub struct DeliveryStore {
    journal: Arc<dyn DeliveryJournalPort>,
}

impl DeliveryStore {
    pub fn new<J>(journal: Arc<J>) -> Self
    where
        J: DeliveryJournalPort + 'static,
    {
        Self { journal }
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
        self.journal
            .publish(AtomicPublication::Create {
                delivery_id: record.delivery_id.clone(),
                manifest: encode_manifest(&manifest)?,
                first_record: JournalRecordBytes {
                    sequence: 1,
                    state: JournalEntryState::Published,
                    bytes: encode_record(&record)?,
                },
            })
            .map_err(map_backend_error)?;
        Ok(DeliveryStoreMutationResult {
            snapshot: record.snapshot,
            replayed: false,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn append(
        &self,
        command: AppendDelivery,
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
            if prior.request_digest != command.request_digest
                || prior.operation != command.operation
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
        if command.expected_revision != stored.snapshot.revision()
            || command.snapshot.revision() != stored.snapshot.revision() + 1
        {
            return Err(store_error(
                DeliveryStoreErrorCode::RevisionConflict,
                "delivery revision changed before mutation",
            ));
        }
        let previous = stored
            .records
            .last()
            .expect("verified journal is non-empty");
        let sequence = parse_sequence(&previous.sequence)? + 1;
        if command.snapshot.revision() != sequence {
            return Err(store_error(
                DeliveryStoreErrorCode::RevisionConflict,
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
                bytes: encode_record(&record)?,
            },
        };
        match self.journal.publish(publication) {
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
                Err(store_error(
                    DeliveryStoreErrorCode::RevisionConflict,
                    "another mutation published this delivery revision",
                ))
            }
            Err(error) => Err(map_backend_error(error)),
        }
    }

    fn read(&self, delivery_id: &DeliveryId) -> Result<StoredDelivery, DeliveryStoreError> {
        let loaded = self
            .journal
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

impl DeliveryCommandPort for DeliveryStore {
    fn execute(
        &self,
        command: DeliveryCommand,
    ) -> Result<DeliveryStoreMutationResult, DeliveryStoreError> {
        match command {
            DeliveryCommand::Create(create) => self.create(create),
            DeliveryCommand::Append(append) => self.append(append),
        }
    }
}

impl DeliveryQueryPort for DeliveryStore {
    fn query(&self, query: DeliveryQuery) -> Result<StoredDelivery, DeliveryStoreError> {
        match query {
            DeliveryQuery::Get(delivery_id) => self.read(&delivery_id),
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
    parse_sequence(&record.sequence)?;
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
    let first = &records[0];
    if first.operation != DeliveryMutationOperation::DeliveryCreated
        || first.digest != manifest.first_record_digest
        || first.snapshot.snapshot().created_at_millis != manifest.created_at_millis
    {
        return Err(store_error(
            DeliveryStoreErrorCode::StoreCorrupt,
            "delivery manifest does not identify its first record",
        ));
    }
    let snapshot = records.last().expect("non-empty checked").snapshot.clone();
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
                let tail_record: DeliveryStoreRecord = serde_json::from_slice(&tail.bytes)
                    .map_err(|error| {
                        JournalBackendError::new(
                            JournalBackendErrorCode::Io,
                            format!("stored record is malformed: {error}"),
                        )
                    })?;
                if tail.sequence != expected_tail_sequence
                    || tail_record.digest != expected_tail_digest
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

    const REQUEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const REQUEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn snapshot(revision: u64, status: &str) -> Delivery {
        let text = include_str!("../tests/fixtures/delivery-store.json")
            .replace("\"revision\": 1", &format!("\"revision\": {revision}"))
            .replace(
                "\"status\": \"draft\"",
                &format!("\"status\": \"{status}\""),
            )
            .replace(
                "\"updatedAtMillis\": 1800000000001",
                &format!("\"updatedAtMillis\": {}", 1_800_000_000_000_u64 + revision),
            );
        Delivery::decode_json(text.as_bytes()).expect("store fixture")
    }

    fn create_store() -> (Arc<InMemoryDeliveryJournal>, DeliveryStore) {
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
}
