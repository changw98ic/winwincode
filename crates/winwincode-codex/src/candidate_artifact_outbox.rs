// SPDX-License-Identifier: Apache-2.0

//! Durable candidate Artifact upload ledger.
//!
//! The exact candidate bytes, descriptor, `artifact.open`, every
//! `artifact.chunk`, and their stable identities are committed before the first
//! transport attempt. A final matching `artifact.ack` is the only transition
//! that exposes the candidate reference to a terminal Job outcome.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use rusqlite::{OptionalExtension as _, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{
    ArtifactId, ExecutionMessageId, ExecutionSequence, Instant, RequestId, SchemaVersion,
    Sha256Digest, WorkerSessionId,
};
use winwincode_execution_port::generated::{
    ArtifactAckMessage, ArtifactChunkMessage, ArtifactChunkMessageKind, ArtifactDescriptor,
    ArtifactKind, ArtifactOpenMessage, ArtifactOpenMessageKind, ArtifactReference, EncodedPayload,
    ExecutionJobReplacementAuthority, ExecutionLeaseStamp, ExecutionPortErrorCode,
    ExecutionPortMessage, ExecutionScope, LeaseWriteStatus,
};

use crate::{
    DurableExecutionDelivery,
    outbox::ExecutionOutbox,
    store::{AdapterStore, AdapterStoreError},
};

/// Canonical candidate product media type.
pub const CANDIDATE_MEDIA_TYPE: &str = "application/vnd.winwincode.git-candidate+json";
/// Canonical candidate product file name.
pub const CANDIDATE_FILE_NAME: &str = "candidate.json";

/// Chat exports the frozen project; Delivery retains its canonical manifest.
#[must_use]
pub fn candidate_artifact_format(profile: &str) -> (&'static str, &'static str) {
    if profile == "codex-chat" {
        ("project.zip", "application/zip")
    } else {
        (CANDIDATE_FILE_NAME, CANDIDATE_MEDIA_TYPE)
    }
}

// A conservative raw ceiling; the actual plan also accounts for canonical
// frame metadata and validates the final remote encoding before retention.
const RAW_CHUNK_BYTES: usize = 64 * 1024;
fn legacy_chunk_bytes() -> usize {
    3 * 1024 * 1024
}
const PENDING: &str = "pending";

/// Exact verified bytes and execution authority entering the durable ledger.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateArtifactUpload {
    pub snapshot_id: Option<winwincode_domain::SnapshotId>,
    pub job_digest: Sha256Digest,
    pub logical_job_digest: Sha256Digest,
    pub execution_profile: String,
    pub scope: ExecutionScope,
    pub replacement_authority: Option<ExecutionJobReplacementAuthority>,
    pub lease: ExecutionLeaseStamp,
    pub worker_session_id: WorkerSessionId,
    pub session_identity: winwincode_domain::SessionIdentity,
    pub bytes: Vec<u8>,
    pub digest: Sha256Digest,
    pub created_at: Instant,
}

impl CandidateArtifactUpload {
    /// Returns the immutable Job/lease/session identity without copying bytes.
    #[must_use]
    pub fn authority(&self) -> CandidateArtifactAuthority {
        CandidateArtifactAuthority {
            snapshot_id: self.snapshot_id.clone(),
            job_digest: self.job_digest.clone(),
            logical_job_digest: self.logical_job_digest.clone(),
            execution_profile: self.execution_profile.clone(),
            scope: self.scope.clone(),
            replacement_authority: self.replacement_authority.clone(),
            lease: self.lease.clone(),
            worker_session_id: self.worker_session_id.clone(),
            session_identity: self.session_identity.clone(),
        }
    }
}

/// Exact candidate upload authority used to recover a final accepted reference.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateArtifactAuthority {
    pub snapshot_id: Option<winwincode_domain::SnapshotId>,
    pub job_digest: Sha256Digest,
    pub logical_job_digest: Sha256Digest,
    pub execution_profile: String,
    pub scope: ExecutionScope,
    pub replacement_authority: Option<ExecutionJobReplacementAuthority>,
    pub lease: ExecutionLeaseStamp,
    pub worker_session_id: WorkerSessionId,
    pub session_identity: winwincode_domain::SessionIdentity,
}

/// First durable retention result.
#[derive(Clone, Debug, PartialEq)]
pub struct RetainedCandidateArtifact {
    pub artifact: ArtifactReference,
    pub authority: CandidateArtifactAuthority,
    pub deliveries: Vec<DurableExecutionDelivery>,
    pub already_accepted: bool,
}

/// Result of applying one exact candidate Artifact acknowledgement.
#[derive(Clone, Debug, PartialEq)]
pub enum CandidateArtifactAckOutcome {
    /// A non-final contiguous prefix was accepted.
    Pending,
    /// The Control Plane requested the original suffix again.
    Replay(Vec<DurableExecutionDelivery>),
    /// The exact final chunk is durable and may enter one Job outcome.
    Accepted(ArtifactReference),
}

/// Candidate Artifact operations over the adapter's one private `SQLite` store.
#[derive(Clone, Debug)]
pub(crate) struct CandidateArtifactOutbox {
    store: AdapterStore,
}

impl CandidateArtifactOutbox {
    pub(crate) fn open(store: AdapterStore) -> Result<Self, AdapterStoreError> {
        store
            .lock()?
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS candidate_artifact_upload (
                   authority_key TEXT PRIMARY KEY NOT NULL,
                   artifact_id TEXT NOT NULL UNIQUE,
                   record_json BLOB NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS candidate_artifact_retired (
                   artifact_id TEXT PRIMARY KEY NOT NULL,
                   successor_artifact_id TEXT NOT NULL UNIQUE,
                   record_json BLOB NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS candidate_artifact_cancelled (
                   artifact_id TEXT PRIMARY KEY NOT NULL,
                   authority_key TEXT NOT NULL,
                   record_json BLOB NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS candidate_cancelled_authority
                   ON candidate_artifact_cancelled(authority_key);
                 CREATE TABLE IF NOT EXISTS candidate_artifact_content (
                   authority_key TEXT PRIMARY KEY NOT NULL,
                   bytes BLOB NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS candidate_artifact_chunks (
                   authority_key TEXT NOT NULL,
                   sequence INTEGER NOT NULL,
                   message_id TEXT NOT NULL UNIQUE,
                   message_json BLOB NOT NULL,
                   PRIMARY KEY(authority_key, sequence)
                 );",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        migrate_candidate_content(&store)?;
        Ok(Self { store })
    }

    pub(crate) fn retain(
        &self,
        upload: &CandidateArtifactUpload,
    ) -> Result<RetainedCandidateArtifact, AdapterStoreError> {
        let record = match StoredCandidateArtifact::from_upload(upload) {
            Ok(record) => record,
            Err(error) => {
                return Err(error);
            }
        };
        self.store.transaction(|transaction| {
            let cancelled: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM candidate_artifact_cancelled WHERE authority_key=?1)",
                [&record.authority_key], |row| row.get(0)).map_err(|_| AdapterStoreError::Unavailable)?;
            if cancelled { return Err(AdapterStoreError::Conflict); }
            if let Some(existing) = load_by_authority(transaction, &record.authority_key)? {
                existing.validate()?;
                if !existing.same_upload(&record) {
                    return Err(AdapterStoreError::Conflict);
                }
                if existing.cancel_requested {
                    return Err(AdapterStoreError::Conflict);
                }
                return Ok(RetainedCandidateArtifact {
                    artifact: existing.reference(),
                    authority: existing.authority(),
                    deliveries: Vec::new(),
                    already_accepted: existing.final_ack.is_some(),
                });
            }
            if let Some(existing) = match replacement_record(transaction, upload) {
                Ok(value) => value,
                Err(error) => {
                    return Err(error);
                }
            } {
                return Ok(RetainedCandidateArtifact {
                    artifact: existing.reference(),
                    authority: existing.authority(),
                    deliveries: Vec::new(),
                    already_accepted: existing.final_ack.is_some(),
                });
            }
            let mut deliveries = Vec::with_capacity(record.chunk_messages.len() + 1);
            deliveries.push(ExecutionOutbox::retain_in_transaction(
                transaction,
                &ExecutionPortMessage::ArtifactOpenMessage(record.open_message.clone()),
            )?);
            for chunk in &record.chunk_messages {
                deliveries.push(ExecutionOutbox::retain_in_transaction(
                    transaction,
                    &ExecutionPortMessage::ArtifactChunkMessage(chunk.clone()),
                )?);
            }
            save_record(transaction, &record)?;
            Ok(RetainedCandidateArtifact {
                artifact: record.reference(),
                authority: record.authority(),
                deliveries,
                already_accepted: false,
            })
        })
    }

    pub(crate) fn apply_ack(
        &self,
        acknowledgement: &ArtifactAckMessage,
    ) -> Result<CandidateArtifactAckOutcome, AdapterStoreError> {
        self.store.transaction(|transaction| {
            let Some(mut record) = load_ack_record(transaction, acknowledgement)? else {
                return Ok(CandidateArtifactAckOutcome::Pending);
            };
            record.validate()?;
            record.validate_ack_authority(acknowledgement)?;
            if record.cancel_requested {
                validate_retired_ack_shape(&record, acknowledgement)?;
                return Ok(CandidateArtifactAckOutcome::Pending);
            }
            if let Some(final_ack) = &record.final_ack {
                return if final_ack_matches_retry(final_ack, acknowledgement) {
                    Ok(CandidateArtifactAckOutcome::Accepted(record.reference()))
                } else {
                    Err(AdapterStoreError::Conflict)
                };
            }
            if acknowledgement.retained_artifact.is_some() {
                if !record.transport_upgrade_pending
                    || !valid_retained_reference(&record, acknowledgement)
                {
                    return Err(AdapterStoreError::Conflict);
                }
                compact_prefix(transaction, &record, record.final_sequence())?;
                record.transport_upgrade_pending = false;
                record.final_ack = Some(acknowledgement.clone());
                record.ack_sequence = 0;
                save_record(transaction, &record)?;
                return Ok(CandidateArtifactAckOutcome::Accepted(record.reference()));
            }
            let acknowledged = u64::try_from(acknowledgement.ack_sequence.0)
                .map_err(|_| AdapterStoreError::Conflict)?;
            let final_sequence = record.final_sequence();
            if acknowledged < record.ack_sequence || acknowledged > final_sequence {
                return Err(AdapterStoreError::Conflict);
            }
            match acknowledgement.status {
                LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate => {
                    if acknowledgement.replay_from_sequence.is_some()
                        || acknowledgement.error.is_some()
                    {
                        return Err(AdapterStoreError::Conflict);
                    }
                    if record.transport_upgrade_pending {
                        if acknowledged != 0
                            || acknowledgement.message_id != record.open_message.message_id
                        {
                            return Err(AdapterStoreError::Conflict);
                        }
                        let replay = queue_upgrade_chunks(transaction, &record)?;
                        record.transport_upgrade_pending = false;
                        compact_prefix(transaction, &record, 0)?;
                        save_record(transaction, &record)?;
                        return Ok(CandidateArtifactAckOutcome::Replay(replay));
                    }
                    compact_prefix(transaction, &record, acknowledged)?;
                    record.ack_sequence = acknowledged;
                    let outcome = if acknowledged == final_sequence {
                        record.final_ack = Some(acknowledgement.clone());
                        CandidateArtifactAckOutcome::Accepted(record.reference())
                    } else {
                        CandidateArtifactAckOutcome::Pending
                    };
                    save_record(transaction, &record)?;
                    Ok(outcome)
                }
                LeaseWriteStatus::Gap => {
                    let replay_from = acknowledgement
                        .replay_from_sequence
                        .as_ref()
                        .and_then(|sequence| u64::try_from(sequence.0).ok())
                        .ok_or(AdapterStoreError::Conflict)?;
                    if acknowledged >= final_sequence
                        || replay_from != acknowledged.saturating_add(1)
                        || acknowledgement.error.is_none()
                    {
                        return Err(AdapterStoreError::Conflict);
                    }
                    compact_prefix(transaction, &record, acknowledged)?;
                    let replay = requeue_suffix(transaction, &record, replay_from)?;
                    record.ack_sequence = acknowledged;
                    save_record(transaction, &record)?;
                    Ok(CandidateArtifactAckOutcome::Replay(replay))
                }
                LeaseWriteStatus::RejectedConflict
                | LeaseWriteStatus::RejectedExpiredLease
                | LeaseWriteStatus::RejectedStaleFencingToken
                | LeaseWriteStatus::RejectedWorkerInstance => Err(AdapterStoreError::Conflict),
            }
        })
    }

    pub(crate) fn accepted_reference(
        &self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<Option<ArtifactReference>, AdapterStoreError> {
        let authority_key = authority_key(
            &authority.lease,
            &authority.worker_session_id,
            &authority.session_identity,
        )?;
        let connection = self.store.lock()?;
        let Some(record) = load_by_authority_connection(&connection, &authority_key)? else {
            return Ok(None);
        };
        record.validate()?;
        if record.job_digest != authority.job_digest
            || record.logical_job_digest != authority.logical_job_digest
            || record.execution_profile != authority.execution_profile
            || record.scope != authority.scope
        {
            return Err(AdapterStoreError::Conflict);
        }
        if record.cancel_requested {
            return Ok(None);
        }
        Ok(record.final_ack.as_ref().map(|_| record.reference()))
    }

    /// Durably records that no further candidate upload frame may be sent.
    ///
    /// The marker is committed before the caller attempts to remove the
    /// retained frames. This makes a cancellation retryable across a process
    /// stop between the intent and the cleanup transaction.
    pub(crate) fn request_cancel(
        &self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<(), AdapterStoreError> {
        self.store.transaction(|transaction| {
            let Some((mut record, exact_authority)) = load_cancel_record(transaction, authority)?
            else {
                return Ok(());
            };
            record.validate()?;
            if (exact_authority && record.job_digest != authority.job_digest)
                || record.logical_job_digest != authority.logical_job_digest
                || record.execution_profile != authority.execution_profile
                || !same_work_contract_item_scope(
                    &record.scope,
                    authority
                        .replacement_authority
                        .as_ref()
                        .map_or(&authority.scope, |proof| &proof.scope),
                )
                || record.final_ack.is_some()
            {
                return Err(AdapterStoreError::Conflict);
            }
            record.cancel_requested = true;
            save_record(transaction, &record)
        })
    }

    /// Returns whether a retained candidate frame is still eligible for send.
    ///
    /// Missing candidate records are treated as ineligible as well: a frame
    /// without its durable artifact record cannot be tied to an active
    /// cancellation/authority decision.
    pub(crate) fn delivery_allowed(
        &self,
        message: &ExecutionPortMessage,
    ) -> Result<bool, AdapterStoreError> {
        let artifact_id = match message {
            ExecutionPortMessage::ArtifactOpenMessage(open)
                if open.artifact.kind == ArtifactKind::Candidate
                    && matches!(
                        open.artifact.media_type.as_str(),
                        CANDIDATE_MEDIA_TYPE | "application/zip"
                    ) =>
            {
                &open.artifact.artifact_id
            }
            ExecutionPortMessage::ArtifactChunkMessage(chunk)
                if matches!(
                    chunk.payload.content_type.as_str(),
                    CANDIDATE_MEDIA_TYPE | "application/zip"
                ) =>
            {
                &chunk.artifact_id
            }
            _ => return Ok(true),
        };
        let connection = self.store.lock()?;
        let Some(record) = load_by_artifact_connection(&connection, artifact_id)? else {
            return Ok(false);
        };
        record.validate()?;
        let exact_frame = match message {
            ExecutionPortMessage::ArtifactOpenMessage(open) => open == &record.open_message,
            ExecutionPortMessage::ArtifactChunkMessage(chunk) => {
                let retained = load_chunk(&connection, &record, chunk.sequence.0)?;
                retained.as_ref() == Some(chunk)
            }
            _ => false,
        };
        if !exact_frame {
            return Err(AdapterStoreError::Conflict);
        }
        Ok(!record.cancel_requested
            && record.final_ack.is_none()
            && (!record.transport_upgrade_pending
                || matches!(message, ExecutionPortMessage::ArtifactOpenMessage(_))))
    }

    pub(crate) fn cancel(
        &self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<(), AdapterStoreError> {
        self.store.transaction(|transaction| {
            let Some((record, exact_authority)) = load_cancel_record(transaction, authority)?
            else {
                return Ok(());
            };
            record.validate()?;
            if (exact_authority && record.job_digest != authority.job_digest)
                || record.logical_job_digest != authority.logical_job_digest
                || record.execution_profile != authority.execution_profile
                || !same_work_contract_item_scope(
                    &record.scope,
                    authority
                        .replacement_authority
                        .as_ref()
                        .map_or(&authority.scope, |proof| &proof.scope),
                )
                || record.final_ack.is_some()
            {
                return Err(AdapterStoreError::Conflict);
            }
            delete_delivery(transaction, &record.open_message.message_id.0)?;
            compact_prefix(transaction, &record, record.final_sequence())?;
            let mut cancelled = record.clone();
            cancelled.cancel_requested = true;
            transaction.execute(
                "INSERT OR IGNORE INTO candidate_artifact_cancelled(artifact_id,authority_key,record_json) VALUES (?1,?2,?3)",
                params![record.descriptor.artifact_id.0, record.authority_key,
                    serde_json::to_vec(&cancelled).map_err(|_| AdapterStoreError::Corrupt)?])
                .map_err(|_| AdapterStoreError::Unavailable)?;
            transaction
                .execute(
                    "DELETE FROM candidate_artifact_chunks WHERE authority_key=?1",
                    [&record.authority_key],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            transaction
                .execute(
                    "DELETE FROM candidate_artifact_content WHERE authority_key=?1",
                    [&record.authority_key],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            let changed = transaction
                .execute(
                    "DELETE FROM candidate_artifact_upload WHERE authority_key = ?1",
                    params![record.authority_key],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            if changed != 1 {
                return Err(AdapterStoreError::Conflict);
            }
            Ok(())
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct StoredCandidateArtifact {
    authority_key: String,
    job_digest: Sha256Digest,
    logical_job_digest: Sha256Digest,
    execution_profile: String,
    scope: ExecutionScope,
    #[serde(default, skip_serializing)]
    bytes: Vec<u8>,
    descriptor: ArtifactDescriptor,
    open_message: ArtifactOpenMessage,
    #[serde(default, skip_serializing)]
    chunk_messages: Vec<ArtifactChunkMessage>,
    #[serde(default = "legacy_chunk_bytes")]
    chunk_size_bytes: usize,
    #[serde(default)]
    chunk_count: u64,
    ack_sequence: u64,
    final_ack: Option<ArtifactAckMessage>,
    #[serde(default)]
    cancel_requested: bool,
    #[serde(default)]
    replacement_authority: Option<ExecutionJobReplacementAuthority>,
    #[serde(default)]
    transport_upgrade_pending: bool,
}

impl StoredCandidateArtifact {
    fn from_upload(upload: &CandidateArtifactUpload) -> Result<Self, AdapterStoreError> {
        Self::from_upload_plan(upload, None)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one immutable wire plan is built and validated before retention"
    )]
    fn from_upload_plan(
        upload: &CandidateArtifactUpload,
        replaces: Option<ArtifactId>,
    ) -> Result<Self, AdapterStoreError> {
        validate_upload(upload)?;
        if let Some(replacement) = upload.replacement_authority.as_ref() {
            validate_replacement_upload(upload, replacement)?;
        }
        let authority_key = authority_key(
            &upload.lease,
            &upload.worker_session_id,
            &upload.session_identity,
        )?;
        let artifact_id = candidate_artifact_id(&authority_key, &upload.digest, replaces.as_ref());
        let size_bytes =
            i64::try_from(upload.bytes.len()).map_err(|_| AdapterStoreError::Conflict)?;
        let (file_name, media_type) = candidate_artifact_format(&upload.execution_profile);
        let descriptor = ArtifactDescriptor {
            artifact_id: artifact_id.clone(),
            digest: upload.digest.clone(),
            file_name: Some(file_name.to_owned()),
            kind: ArtifactKind::Candidate,
            media_type: media_type.to_owned(),
            size_bytes,
        };
        let open_message = ArtifactOpenMessage {
            replaces_artifact_id: replaces,
            artifact: descriptor.clone(),
            kind: ArtifactOpenMessageKind::ArtifactOpen,
            lease: upload.lease.clone(),
            message_id: ExecutionMessageId(canonical_id(
                "xmsg",
                b"winwincode.candidate-artifact.open-message.v1",
                &[artifact_id.0.as_bytes()],
            )),
            request_id: RequestId(canonical_id(
                "req",
                b"winwincode.candidate-artifact.open-request.v1",
                &[artifact_id.0.as_bytes()],
            )),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: upload.created_at.clone(),
            session_identity: upload.session_identity.clone(),
            snapshot_id: upload.snapshot_id.clone(),
            worker_session_id: upload.worker_session_id.clone(),
        };
        let chunk_size_bytes = chunk_budget(&open_message)?;
        let chunk_messages = upload
            .bytes
            .chunks(chunk_size_bytes)
            .enumerate()
            .map(|(index, bytes)| {
                let sequence = u64::try_from(index)
                    .ok()
                    .and_then(|index| index.checked_add(1))
                    .ok_or(AdapterStoreError::Conflict)?;
                let sequence_i64 =
                    i64::try_from(sequence).map_err(|_| AdapterStoreError::Conflict)?;
                let payload_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)));
                Ok(ArtifactChunkMessage {
                    artifact_id: artifact_id.clone(),
                    is_final: index + 1 == upload.bytes.chunks(chunk_size_bytes).len(),
                    kind: ArtifactChunkMessageKind::ArtifactChunk,
                    lease: upload.lease.clone(),
                    message_id: ExecutionMessageId(canonical_id(
                        "xmsg",
                        b"winwincode.candidate-artifact.chunk-message.v1",
                        &[artifact_id.0.as_bytes(), &sequence.to_be_bytes()],
                    )),
                    payload: EncodedPayload {
                        content_type: descriptor.media_type.clone(),
                        data_base64: BASE64_STANDARD.encode(bytes),
                        payload_digest,
                    },
                    schema_version: SchemaVersion::WinwincodeV1,
                    sent_at: upload.created_at.clone(),
                    sequence: ExecutionSequence(sequence_i64),
                    session_identity: upload.session_identity.clone(),
                    snapshot_id: upload.snapshot_id.clone(),
                    worker_session_id: upload.worker_session_id.clone(),
                })
            })
            .collect::<Result<Vec<_>, AdapterStoreError>>()?;
        let record = Self {
            authority_key,
            job_digest: upload.job_digest.clone(),
            logical_job_digest: upload.logical_job_digest.clone(),
            execution_profile: upload.execution_profile.clone(),
            scope: upload.scope.clone(),
            bytes: upload.bytes.clone(),
            descriptor,
            open_message,
            chunk_count: u64::try_from(chunk_messages.len())
                .map_err(|_| AdapterStoreError::Conflict)?,
            chunk_size_bytes,
            chunk_messages,
            ack_sequence: 0,
            final_ack: None,
            cancel_requested: false,
            replacement_authority: upload.replacement_authority.clone(),
            transport_upgrade_pending: false,
        };
        record.validate()?;
        check_remote_candidate_frame(
            &ExecutionPortMessage::ArtifactOpenMessage(record.open_message.clone()),
            &record.open_message,
        )?;
        for chunk in &record.chunk_messages {
            check_remote_candidate_frame(
                &ExecutionPortMessage::ArtifactChunkMessage(chunk.clone()),
                &record.open_message,
            )?;
        }
        Ok(record)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "candidate identity, progress, and immutable content are checked together"
    )]
    fn validate(&self) -> Result<(), AdapterStoreError> {
        let final_sequence = self.final_sequence();
        let exact_authority_key = authority_key(
            &self.open_message.lease,
            &self.open_message.worker_session_id,
            &self.open_message.session_identity,
        )?;
        let exact_artifact_id = candidate_artifact_id(
            &exact_authority_key,
            &self.descriptor.digest,
            self.open_message.replaces_artifact_id.as_ref(),
        );
        if !winwincode_execution_port::snapshot_freeze::snapshot_role_binding_valid(
            &self.execution_profile,
            self.open_message.snapshot_id.as_ref(),
        ) || !candidate_artifact_role(&self.execution_profile)
            || self.descriptor.size_bytes <= 0
            || self.chunk_size_bytes == 0
            || !lowercase_sha256(&self.job_digest.0)
            || !lowercase_sha256(&self.logical_job_digest.0)
            || !scope_matches_session(&self.scope, &self.open_message.session_identity)
            || self.authority_key != exact_authority_key
            || !lowercase_sha256(&self.descriptor.digest.0)
            || self.descriptor.artifact_id != exact_artifact_id
            || self.descriptor.kind != ArtifactKind::Candidate
            || self.descriptor.media_type != candidate_artifact_format(&self.execution_profile).1
            || self.descriptor.file_name.as_deref()
                != Some(candidate_artifact_format(&self.execution_profile).0)
            || self.open_message.artifact != self.descriptor
            || self.open_message.kind != ArtifactOpenMessageKind::ArtifactOpen
            || self.open_message.schema_version != SchemaVersion::WinwincodeV1
            || self.open_message.worker_session_id
                != self.open_message.session_identity.worker_session_id
            || self.open_message.message_id.0
                != canonical_id(
                    "xmsg",
                    b"winwincode.candidate-artifact.open-message.v1",
                    &[self.descriptor.artifact_id.0.as_bytes()],
                )
            || self.open_message.request_id.0
                != canonical_id(
                    "req",
                    b"winwincode.candidate-artifact.open-request.v1",
                    &[self.descriptor.artifact_id.0.as_bytes()],
                )
            || self.chunk_count == 0
            || u64::try_from(self.descriptor.size_bytes)
                .ok()
                .map(|size| size.div_ceil(self.chunk_size_bytes as u64))
                != Some(self.chunk_count)
            || self.ack_sequence > final_sequence
            || (self.cancel_requested && self.final_ack.is_some())
        {
            return Err(AdapterStoreError::Corrupt);
        }
        // Immutable material is checked once at retention/recovery, never on
        // each ACK or eligibility lookup. Those paths load metadata only.
        if !self.bytes.is_empty()
            && (i64::try_from(self.bytes.len()).map_err(|_| AdapterStoreError::Corrupt)?
                != self.descriptor.size_bytes
                || format!("sha256:{:x}", Sha256::digest(&self.bytes)) != self.descriptor.digest.0
                || self.rebuild_chunk_bytes()? != self.bytes)
        {
            return Err(AdapterStoreError::Corrupt);
        }
        if let Some(replacement) = &self.replacement_authority {
            if replacement.successor_lease != self.open_message.lease
                || replacement.predecessor_lease.job_id != self.open_message.lease.job_id
                || replacement.predecessor_lease.attempt.saturating_add(1)
                    != self.open_message.lease.attempt
                || replacement.predecessor_lease.worker_id != self.open_message.lease.worker_id
                || replacement.predecessor_lease.worker_instance_id
                    == self.open_message.lease.worker_instance_id
                || replacement.logical_job_digest != self.logical_job_digest
                || !same_work_contract_item_scope(&replacement.scope, &self.scope)
                || !different_work_run(&replacement.scope, &self.scope)
                || !lowercase_sha256(&replacement.receipt_digest.0)
                || !lowercase_sha256(&replacement.logical_job_digest.0)
            {
                return Err(AdapterStoreError::Corrupt);
            }
            if let Some(predecessor_session) = replacement.predecessor_session_identity.as_ref()
                && (predecessor_session.worker_session_id == self.open_message.worker_session_id
                    || !same_predecessor_session_scope(predecessor_session, &replacement.scope)
                    || predecessor_session.work_run_id
                        == self.open_message.session_identity.work_run_id)
            {
                return Err(AdapterStoreError::Corrupt);
            }
        }
        if let Some(ack) = &self.final_ack {
            self.validate_ack_authority(ack)?;
            if !matches!(
                ack.status,
                LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
            ) || u64::try_from(ack.ack_sequence.0).ok()
                != Some(if ack.retained_artifact.is_some() {
                    0
                } else {
                    final_sequence
                })
                || ack.replay_from_sequence.is_some()
                || ack.error.is_some()
                || self.ack_sequence
                    != if ack.retained_artifact.is_some() {
                        0
                    } else {
                        final_sequence
                    }
                || (ack.retained_artifact.is_some() && !valid_retained_reference(self, ack))
            {
                return Err(AdapterStoreError::Corrupt);
            }
        }
        Ok(())
    }

    fn rebuild_chunk_bytes(&self) -> Result<Vec<u8>, AdapterStoreError> {
        if self.chunk_messages.len() != self.bytes.chunks(self.chunk_size_bytes).len() {
            return Err(AdapterStoreError::Corrupt);
        }
        self.chunk_messages
            .iter()
            .zip(self.bytes.chunks(self.chunk_size_bytes))
            .enumerate()
            .try_fold(Vec::new(), |mut bytes, (index, (chunk, expected_bytes))| {
                let expected = i64::try_from(index + 1).map_err(|_| AdapterStoreError::Corrupt)?;
                let decoded = BASE64_STANDARD
                    .decode(&chunk.payload.data_base64)
                    .map_err(|_| AdapterStoreError::Corrupt)?;
                let sequence = u64::try_from(expected).map_err(|_| AdapterStoreError::Corrupt)?;
                if decoded.as_slice() != expected_bytes
                    || chunk.artifact_id != self.descriptor.artifact_id
                    || chunk.snapshot_id != self.open_message.snapshot_id
                    || chunk.lease != self.open_message.lease
                    || chunk.worker_session_id != self.open_message.worker_session_id
                    || chunk.session_identity != self.open_message.session_identity
                    || chunk.schema_version != SchemaVersion::WinwincodeV1
                    || chunk.kind != ArtifactChunkMessageKind::ArtifactChunk
                    || chunk.sent_at != self.open_message.sent_at
                    || chunk.sequence.0 != expected
                    || chunk.message_id.0
                        != canonical_id(
                            "xmsg",
                            b"winwincode.candidate-artifact.chunk-message.v1",
                            &[
                                self.descriptor.artifact_id.0.as_bytes(),
                                &sequence.to_be_bytes(),
                            ],
                        )
                    || chunk.is_final != (index + 1 == self.chunk_messages.len())
                    || chunk.payload.content_type != self.descriptor.media_type
                    || chunk.payload.payload_digest.0
                        != format!("sha256:{:x}", Sha256::digest(&decoded))
                {
                    return Err(AdapterStoreError::Corrupt);
                }
                bytes.extend(decoded);
                Ok(bytes)
            })
    }

    fn validate_ack_authority(
        &self,
        acknowledgement: &ArtifactAckMessage,
    ) -> Result<(), AdapterStoreError> {
        if acknowledgement.schema_version != SchemaVersion::WinwincodeV1
            || acknowledgement.artifact_id != self.descriptor.artifact_id
            || acknowledgement.lease != self.open_message.lease
            || acknowledgement.worker_session_id != self.open_message.worker_session_id
            || acknowledgement.session_identity != self.open_message.session_identity
        {
            return Err(AdapterStoreError::Conflict);
        }
        Ok(())
    }

    fn same_upload(&self, other: &Self) -> bool {
        self.authority_key == other.authority_key
            && self.open_message.snapshot_id == other.open_message.snapshot_id
            && self.job_digest == other.job_digest
            && self.logical_job_digest == other.logical_job_digest
            && self.execution_profile == other.execution_profile
            && self.scope == other.scope
            && self.descriptor.digest == other.descriptor.digest
            && self.descriptor.size_bytes == other.descriptor.size_bytes
            && self.descriptor.kind == other.descriptor.kind
            && self.descriptor.media_type == other.descriptor.media_type
            && self.descriptor.file_name == other.descriptor.file_name
            && self.replacement_authority == other.replacement_authority
    }

    fn final_sequence(&self) -> u64 {
        self.chunk_count
    }

    fn reference(&self) -> ArtifactReference {
        if let Some(reference) = self
            .final_ack
            .as_ref()
            .and_then(|ack| ack.retained_artifact.as_ref())
        {
            return reference.clone();
        }
        ArtifactReference {
            artifact_id: self.descriptor.artifact_id.clone(),
            digest: self.descriptor.digest.clone(),
        }
    }

    fn authority(&self) -> CandidateArtifactAuthority {
        CandidateArtifactAuthority {
            snapshot_id: self.open_message.snapshot_id.clone(),
            job_digest: self.job_digest.clone(),
            logical_job_digest: self.logical_job_digest.clone(),
            execution_profile: self.execution_profile.clone(),
            scope: self.scope.clone(),
            replacement_authority: self.replacement_authority.clone(),
            lease: self.open_message.lease.clone(),
            worker_session_id: self.open_message.worker_session_id.clone(),
            session_identity: self.open_message.session_identity.clone(),
        }
    }
}

fn replacement_record(
    transaction: &Transaction<'_>,
    upload: &CandidateArtifactUpload,
) -> Result<Option<StoredCandidateArtifact>, AdapterStoreError> {
    let Some(replacement) = upload.replacement_authority.as_ref() else {
        return Ok(None);
    };
    validate_replacement_upload(upload, replacement)?;
    let Some(predecessor_session) = replacement.predecessor_session_identity.as_ref() else {
        return Ok(None);
    };
    let predecessor_key = authority_key(
        &replacement.predecessor_lease,
        &predecessor_session.worker_session_id,
        predecessor_session,
    )?;
    let Some(record) = load_by_authority(transaction, &predecessor_key)? else {
        return Ok(None);
    };
    record.validate()?;
    if record.cancel_requested {
        return Err(AdapterStoreError::Conflict);
    }
    if record.logical_job_digest != upload.logical_job_digest
        || record.execution_profile != upload.execution_profile
        || !same_work_run_identity(&record.scope, &upload.scope)
        || record.descriptor.size_bytes
            != i64::try_from(upload.bytes.len()).map_err(|_| AdapterStoreError::Conflict)?
        || record.descriptor.digest != upload.digest
    {
        return Err(AdapterStoreError::Conflict);
    }
    Ok(Some(record))
}

fn same_work_contract_item_scope(left: &ExecutionScope, right: &ExecutionScope) -> bool {
    match (left, right) {
        (ExecutionScope::WorkRunExecutionScope(a), ExecutionScope::WorkRunExecutionScope(b)) => {
            a.product_session_id == b.product_session_id
                && a.work_contract_id == b.work_contract_id
                && a.work_contract_revision == b.work_contract_revision
                && a.work_item_id == b.work_item_id
                && a.work_item_revision == b.work_item_revision
        }
        (
            ExecutionScope::ProductSessionExecutionScope(a),
            ExecutionScope::ProductSessionExecutionScope(b),
        ) => a == b,
        _ => false,
    }
}

fn different_work_run(left: &ExecutionScope, right: &ExecutionScope) -> bool {
    match (left, right) {
        (ExecutionScope::WorkRunExecutionScope(a), ExecutionScope::WorkRunExecutionScope(b)) => {
            a.work_run_id != b.work_run_id
        }
        _ => false,
    }
}

fn same_predecessor_session_scope(
    session: &winwincode_domain::SessionIdentity,
    scope: &ExecutionScope,
) -> bool {
    match scope {
        ExecutionScope::WorkRunExecutionScope(scope) => {
            session.product_session_id == scope.product_session_id
                && session.work_run_id.as_ref() == Some(&scope.work_run_id)
        }
        ExecutionScope::ProductSessionExecutionScope(scope) => {
            session.product_session_id == scope.product_session_id && session.work_run_id.is_none()
        }
    }
}

fn same_work_run_identity(left: &ExecutionScope, right: &ExecutionScope) -> bool {
    match (left, right) {
        (
            ExecutionScope::ProductSessionExecutionScope(a),
            ExecutionScope::ProductSessionExecutionScope(b),
        ) => a == b,
        (ExecutionScope::WorkRunExecutionScope(a), ExecutionScope::WorkRunExecutionScope(b)) => {
            a.product_session_id == b.product_session_id
                && a.work_contract_id == b.work_contract_id
                && a.work_contract_revision == b.work_contract_revision
                && a.work_item_id == b.work_item_id
                && a.work_item_revision == b.work_item_revision
                && a.work_run_id != b.work_run_id
        }
        _ => false,
    }
}

fn validate_upload(upload: &CandidateArtifactUpload) -> Result<(), AdapterStoreError> {
    let actual_digest = format!("sha256:{:x}", Sha256::digest(&upload.bytes));
    let valid_scope = scope_matches_session(&upload.scope, &upload.session_identity);
    if !winwincode_execution_port::snapshot_freeze::snapshot_role_binding_valid(
        &upload.execution_profile,
        upload.snapshot_id.as_ref(),
    ) || !candidate_artifact_role(&upload.execution_profile)
        || upload.bytes.is_empty()
        || upload.digest.0 != actual_digest
        || !lowercase_sha256(&upload.job_digest.0)
        || !lowercase_sha256(&upload.logical_job_digest.0)
        || !valid_scope
        || upload.lease.attempt <= 0
        || upload.worker_session_id != upload.session_identity.worker_session_id
        || upload.created_at.0 < upload.lease.issued_at.0
        || upload.created_at.0 >= upload.lease.expires_at.0
    {
        return Err(AdapterStoreError::Conflict);
    }
    Ok(())
}

fn validate_replacement_upload(
    upload: &CandidateArtifactUpload,
    replacement: &ExecutionJobReplacementAuthority,
) -> Result<(), AdapterStoreError> {
    if replacement.successor_lease != upload.lease
        || replacement.predecessor_lease.job_id != upload.lease.job_id
        || replacement.predecessor_lease.attempt.saturating_add(1) != upload.lease.attempt
        || replacement.predecessor_lease.worker_id != upload.lease.worker_id
        || replacement.predecessor_lease.worker_instance_id == upload.lease.worker_instance_id
        || replacement.logical_job_digest != upload.logical_job_digest
        || !same_work_run_identity(&replacement.scope, &upload.scope)
        || !lowercase_sha256(&replacement.receipt_digest.0)
        || !lowercase_sha256(&replacement.logical_job_digest.0)
    {
        return Err(AdapterStoreError::Conflict);
    }
    Ok(())
}

fn candidate_artifact_role(profile: &str) -> bool {
    matches!(
        profile,
        "codex-chat" | "executor" | "remediator" | "reviewer" | "verifier" | "adversarial-verifier"
    )
}

fn scope_matches_session(
    scope: &ExecutionScope,
    session: &winwincode_domain::SessionIdentity,
) -> bool {
    match scope {
        ExecutionScope::ProductSessionExecutionScope(scope) => {
            session.product_session_id == scope.product_session_id && session.work_run_id.is_none()
        }
        ExecutionScope::WorkRunExecutionScope(scope) => {
            session.product_session_id == scope.product_session_id
                && session.work_run_id.as_ref() == Some(&scope.work_run_id)
        }
    }
}

fn lowercase_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

fn authority_key(
    lease: &ExecutionLeaseStamp,
    worker_session_id: &WorkerSessionId,
    session_identity: &winwincode_domain::SessionIdentity,
) -> Result<String, AdapterStoreError> {
    let bytes = serde_json::to_vec(&(lease, worker_session_id, session_identity))
        .map_err(|_| AdapterStoreError::Corrupt)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn save_record(
    transaction: &Transaction<'_>,
    record: &StoredCandidateArtifact,
) -> Result<(), AdapterStoreError> {
    if !record.bytes.is_empty() {
        transaction
            .execute(
                "INSERT INTO candidate_artifact_content(authority_key,bytes) VALUES (?1,?2)",
                params![record.authority_key, record.bytes],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        for chunk in &record.chunk_messages {
            let bytes = serde_json::to_vec(chunk).map_err(|_| AdapterStoreError::Corrupt)?;
            transaction.execute("INSERT INTO candidate_artifact_chunks(authority_key,sequence,message_id,message_json) VALUES (?1,?2,?3,?4)", params![record.authority_key, chunk.sequence.0, chunk.message_id.0, bytes]).map_err(|_| AdapterStoreError::Unavailable)?;
        }
    }
    let bytes = serde_json::to_vec(record).map_err(|_| AdapterStoreError::Corrupt)?;
    transaction
        .execute(
            "INSERT INTO candidate_artifact_upload(authority_key, artifact_id, record_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(authority_key) DO UPDATE SET record_json = excluded.record_json, artifact_id = excluded.artifact_id",
            params![
                &record.authority_key,
                &record.descriptor.artifact_id.0,
                bytes
            ],
        )
        .map_err(|_| AdapterStoreError::Unavailable)?;
    Ok(())
}

fn load_by_authority(
    transaction: &Transaction<'_>,
    authority_key: &str,
) -> Result<Option<StoredCandidateArtifact>, AdapterStoreError> {
    let bytes = transaction
        .query_row(
            "SELECT record_json FROM candidate_artifact_upload WHERE authority_key = ?1",
            params![authority_key],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    decode_record(bytes)
}

fn load_by_authority_connection(
    connection: &rusqlite::Connection,
    authority_key: &str,
) -> Result<Option<StoredCandidateArtifact>, AdapterStoreError> {
    let bytes = connection
        .query_row(
            "SELECT record_json FROM candidate_artifact_upload WHERE authority_key = ?1",
            params![authority_key],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    decode_record(bytes)
}

/// Finds the upload owned by one active authority, including a predecessor
/// stream sealed for a one-attempt replacement. The latter is needed when a
/// Worker restarts after committing cancellation intent: its in-memory
/// predecessor completion is gone, but the successor lease still identifies
/// the exact predecessor attempt that must be cleaned up.
fn load_cancel_record(
    transaction: &Transaction<'_>,
    authority: &CandidateArtifactAuthority,
) -> Result<Option<(StoredCandidateArtifact, bool)>, AdapterStoreError> {
    let exact_key = authority_key(
        &authority.lease,
        &authority.worker_session_id,
        &authority.session_identity,
    )?;
    if let Some(record) = load_by_authority(transaction, &exact_key)? {
        return Ok(Some((record, true)));
    }
    let Some(proof) = authority.replacement_authority.as_ref() else {
        return Ok(None);
    };
    let Some(predecessor_session) = proof.predecessor_session_identity.as_ref() else {
        return Ok(None);
    };
    if proof.successor_lease != authority.lease
        || proof.logical_job_digest != authority.logical_job_digest
        || proof.predecessor_lease.attempt.saturating_add(1) != authority.lease.attempt
        || proof.predecessor_lease.worker_id != authority.lease.worker_id
        || proof.predecessor_lease.worker_instance_id == authority.lease.worker_instance_id
    {
        return Err(AdapterStoreError::Conflict);
    }
    let predecessor_key = authority_key(
        &proof.predecessor_lease,
        &predecessor_session.worker_session_id,
        predecessor_session,
    )?;
    let Some(record) = load_by_authority(transaction, &predecessor_key)? else {
        return Ok(None);
    };
    record.validate()?;
    if record.logical_job_digest != authority.logical_job_digest
        || record.execution_profile != authority.execution_profile
        || record.scope != proof.scope
        || record.open_message.lease != proof.predecessor_lease
        || record.open_message.worker_session_id != predecessor_session.worker_session_id
        || record.open_message.session_identity != *predecessor_session
    {
        return Err(AdapterStoreError::Conflict);
    }
    Ok(Some((record, false)))
}

// Late acknowledgements of a retired transport plan never advance its successor.
fn load_ack_record(
    transaction: &Transaction<'_>,
    acknowledgement: &ArtifactAckMessage,
) -> Result<Option<StoredCandidateArtifact>, AdapterStoreError> {
    if let Some(record) = load_by_artifact(transaction, &acknowledgement.artifact_id)? {
        return Ok(Some(record));
    }
    let bytes: Option<Vec<u8>> = transaction
        .query_row(
            "SELECT record_json FROM candidate_artifact_retired WHERE artifact_id=?1
             UNION ALL SELECT record_json FROM candidate_artifact_cancelled WHERE artifact_id=?1 LIMIT 1",
            [&acknowledgement.artifact_id.0],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let retired = decode_record(bytes)?.ok_or(AdapterStoreError::Conflict)?;
    retired.validate()?;
    retired.validate_ack_authority(acknowledgement)?;
    validate_retired_ack_shape(&retired, acknowledgement)?;
    Ok(None)
}

// Retired/cancelled uploads consume exact, well-formed late receipts without
// applying progress or replaying chunks. Malformed/foreign receipts still fail.
fn validate_retired_ack_shape(
    record: &StoredCandidateArtifact,
    ack: &ArtifactAckMessage,
) -> Result<(), AdapterStoreError> {
    let sequence = u64::try_from(ack.ack_sequence.0).map_err(|_| AdapterStoreError::Conflict)?;
    if sequence > record.final_sequence() {
        return Err(AdapterStoreError::Conflict);
    }
    if ack.retained_artifact.is_some() {
        return if record.transport_upgrade_pending && valid_retained_reference(record, ack) {
            Ok(())
        } else {
            Err(AdapterStoreError::Conflict)
        };
    }
    let rejection_code = match ack.status {
        LeaseWriteStatus::RejectedExpiredLease => Some(ExecutionPortErrorCode::LeaseExpired),
        LeaseWriteStatus::RejectedStaleFencingToken => {
            Some(ExecutionPortErrorCode::StaleFencingToken)
        }
        LeaseWriteStatus::RejectedWorkerInstance => {
            Some(ExecutionPortErrorCode::WorkerInstanceChanged)
        }
        _ => None,
    };
    if let Some(code) = rejection_code {
        return if ack.replay_from_sequence.is_none()
            && ack
                .error
                .as_ref()
                .is_some_and(|error| error.code == code && !error.retryable)
        {
            Ok(())
        } else {
            Err(AdapterStoreError::Conflict)
        };
    }
    match ack.status {
        LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
            if ack.error.is_none() && ack.replay_from_sequence.is_none() =>
        {
            Ok(())
        }
        LeaseWriteStatus::Gap
            if sequence < record.final_sequence()
                && ack.error.is_some()
                && ack
                    .replay_from_sequence
                    .as_ref()
                    .is_some_and(|from| u64::try_from(from.0).ok() == Some(sequence + 1)) =>
        {
            Ok(())
        }
        _ => Err(AdapterStoreError::Conflict),
    }
}

fn load_by_artifact(
    transaction: &Transaction<'_>,
    artifact_id: &ArtifactId,
) -> Result<Option<StoredCandidateArtifact>, AdapterStoreError> {
    let bytes = transaction
        .query_row(
            "SELECT record_json FROM candidate_artifact_upload WHERE artifact_id = ?1",
            params![&artifact_id.0],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    decode_record(bytes)
}

fn load_by_artifact_connection(
    connection: &rusqlite::Connection,
    artifact_id: &ArtifactId,
) -> Result<Option<StoredCandidateArtifact>, AdapterStoreError> {
    let bytes = connection
        .query_row(
            "SELECT record_json FROM candidate_artifact_upload WHERE artifact_id = ?1",
            params![&artifact_id.0],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    decode_record(bytes)
}

fn decode_record(
    bytes: Option<Vec<u8>>,
) -> Result<Option<StoredCandidateArtifact>, AdapterStoreError> {
    bytes
        .map(|bytes| {
            let mut record: StoredCandidateArtifact =
                serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
            if record.chunk_count == 0 && !record.chunk_messages.is_empty() {
                record.chunk_count = record.chunk_messages.len() as u64;
            }
            Ok(record)
        })
        .transpose()
}

fn load_chunk(
    connection: &rusqlite::Connection,
    record: &StoredCandidateArtifact,
    sequence: i64,
) -> Result<Option<ArtifactChunkMessage>, AdapterStoreError> {
    let bytes: Option<Vec<u8>> = connection.query_row("SELECT message_json FROM candidate_artifact_chunks WHERE authority_key=?1 AND sequence=?2", params![record.authority_key,sequence], |r| r.get(0)).optional().map_err(|_| AdapterStoreError::Unavailable)?;
    bytes
        .map(|bytes| {
            let chunk: ArtifactChunkMessage =
                serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
            if sequence < 1
                || u64::try_from(sequence)
                    .ok()
                    .is_none_or(|sequence| sequence > record.chunk_count)
            {
                return Err(AdapterStoreError::Corrupt);
            }
            let raw = BASE64_STANDARD
                .decode(&chunk.payload.data_base64)
                .map_err(|_| AdapterStoreError::Corrupt)?;
            if raw.len() > record.chunk_size_bytes
                || raw.is_empty()
                || chunk.artifact_id != record.descriptor.artifact_id
                || chunk.lease != record.open_message.lease
                || chunk.worker_session_id != record.open_message.worker_session_id
                || chunk.session_identity != record.open_message.session_identity
                || chunk.sequence.0 != sequence
                || chunk.is_final != (u64::try_from(sequence).ok() == Some(record.chunk_count))
                || chunk.payload.content_type != record.descriptor.media_type
                || chunk.payload.payload_digest.0 != format!("sha256:{:x}", Sha256::digest(&raw))
                || chunk.snapshot_id != record.open_message.snapshot_id
                || chunk.sent_at != record.open_message.sent_at
                || chunk.schema_version != SchemaVersion::WinwincodeV1
                || chunk.message_id.0
                    != canonical_id(
                        "xmsg",
                        b"winwincode.candidate-artifact.chunk-message.v1",
                        &[
                            record.descriptor.artifact_id.0.as_bytes(),
                            &u64::try_from(sequence)
                                .map_err(|_| AdapterStoreError::Corrupt)?
                                .to_be_bytes(),
                        ],
                    )
            {
                return Err(AdapterStoreError::Corrupt);
            }
            Ok(chunk)
        })
        .transpose()
}

fn migrate_candidate_content(store: &AdapterStore) -> Result<(), AdapterStoreError> {
    store.transaction(|tx| {
        let rows = {
            let mut statement = tx
                .prepare("SELECT authority_key FROM candidate_artifact_upload")
                .map_err(|_| AdapterStoreError::Unavailable)?;

            statement
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|_| AdapterStoreError::Unavailable)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| AdapterStoreError::Unavailable)?
        };
        for key in rows {
            let mut record = load_by_authority(tx, &key)?.ok_or(AdapterStoreError::Corrupt)?;
            if record.bytes.is_empty() {
                record.bytes = tx
                    .query_row(
                        "SELECT bytes FROM candidate_artifact_content WHERE authority_key=?1",
                        [&record.authority_key],
                        |r| r.get(0),
                    )
                    .map_err(|_| AdapterStoreError::Corrupt)?;
                record.chunk_messages = (1..=record.chunk_count)
                    .map(|sequence| {
                        load_chunk(
                            tx,
                            &record,
                            i64::try_from(sequence).map_err(|_| AdapterStoreError::Corrupt)?,
                        )?
                        .ok_or(AdapterStoreError::Corrupt)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                record.validate()?;
            } else {
                record.validate()?;
                save_record(tx, &record)?;
            }
            if record.final_ack.is_none()
                && !record.cancel_requested
                && record.chunk_messages.iter().any(|chunk| {
                    check_remote_candidate_frame(
                        &ExecutionPortMessage::ArtifactChunkMessage(chunk.clone()),
                        &record.open_message,
                    )
                    .is_err()
                })
            {
                upgrade_transport_plan(tx, &record)?;
            }
        }
        Ok(())
    })
}

fn upgrade_transport_plan(
    tx: &Transaction<'_>,
    old: &StoredCandidateArtifact,
) -> Result<(), AdapterStoreError> {
    let upload = CandidateArtifactUpload {
        snapshot_id: old.open_message.snapshot_id.clone(),
        job_digest: old.job_digest.clone(),
        logical_job_digest: old.logical_job_digest.clone(),
        execution_profile: old.execution_profile.clone(),
        scope: old.scope.clone(),
        lease: old.open_message.lease.clone(),
        worker_session_id: old.open_message.worker_session_id.clone(),
        session_identity: old.open_message.session_identity.clone(),
        digest: old.descriptor.digest.clone(),
        bytes: old.bytes.clone(),
        created_at: old.open_message.sent_at.clone(),
        replacement_authority: old.replacement_authority.clone(),
    };
    let mut replacement = StoredCandidateArtifact::from_upload_plan(
        &upload,
        Some(old.descriptor.artifact_id.clone()),
    )?;
    replacement.transport_upgrade_pending = true;
    compact_prefix(tx, old, old.final_sequence())?;
    tx.execute(
        "INSERT INTO candidate_artifact_retired VALUES(?1,?2,?3)",
        params![
            old.descriptor.artifact_id.0,
            replacement.descriptor.artifact_id.0,
            serde_json::to_vec(old).map_err(|_| AdapterStoreError::Corrupt)?
        ],
    )
    .map_err(|_| AdapterStoreError::Unavailable)?;
    tx.execute(
        "DELETE FROM candidate_artifact_chunks WHERE authority_key=?1",
        [&old.authority_key],
    )
    .map_err(|_| AdapterStoreError::Unavailable)?;
    tx.execute(
        "DELETE FROM candidate_artifact_content WHERE authority_key=?1",
        [&old.authority_key],
    )
    .map_err(|_| AdapterStoreError::Unavailable)?;
    save_record(tx, &replacement)?;
    ExecutionOutbox::retain_in_transaction(
        tx,
        &ExecutionPortMessage::ArtifactOpenMessage(replacement.open_message.clone()),
    )?;
    Ok(())
}

fn candidate_artifact_id(
    authority: &str,
    digest: &Sha256Digest,
    predecessor: Option<&ArtifactId>,
) -> ArtifactId {
    ArtifactId(match predecessor {
        None => canonical_id(
            "art",
            b"winwincode.candidate-artifact.v1",
            &[authority.as_bytes(), digest.0.as_bytes()],
        ),
        Some(old) => canonical_id(
            "art",
            b"winwincode.candidate-artifact.transport-plan.v2",
            &[authority.as_bytes(), digest.0.as_bytes(), old.0.as_bytes()],
        ),
    })
}

fn valid_retained_reference(record: &StoredCandidateArtifact, ack: &ArtifactAckMessage) -> bool {
    ack.retained_artifact.as_ref().is_some_and(|reference| {
        Some(&reference.artifact_id) == record.open_message.replaces_artifact_id.as_ref()
            && reference.digest == record.descriptor.digest
    }) && ack.message_id == record.open_message.message_id
        && ack.ack_sequence.0 == 0
        && matches!(
            ack.status,
            LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
        )
        && ack.error.is_none()
        && ack.replay_from_sequence.is_none()
}

fn chunk_budget(open: &ArtifactOpenMessage) -> Result<usize, AdapterStoreError> {
    use winwincode_execution_port::transport::{
        FrameDirection, MAX_REMOTE_FRAME_BYTES, RemoteExchangeRequest, RemoteTransportAdapter,
        TypedFrame,
    };
    let chunk = ArtifactChunkMessage {
        artifact_id: open.artifact.artifact_id.clone(),
        is_final: false,
        kind: ArtifactChunkMessageKind::ArtifactChunk,
        lease: open.lease.clone(),
        message_id: ExecutionMessageId("xmsg_00000000000000000000000000".into()),
        payload: EncodedPayload {
            content_type: open.artifact.media_type.clone(),
            data_base64: String::new(),
            payload_digest: Sha256Digest(format!("sha256:{}", "0".repeat(64))),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: open.sent_at.clone(),
        sequence: ExecutionSequence(i64::MAX),
        session_identity: open.session_identity.clone(),
        snapshot_id: open.snapshot_id.clone(),
        worker_session_id: open.worker_session_id.clone(),
    };
    let frame = TypedFrame::new(
        FrameDirection::WorkerToControlPlane,
        ExecutionPortMessage::ArtifactChunkMessage(chunk),
    )
    .and_then(|f| RemoteTransportAdapter::<CandidateNoopCore>::encode(&f))
    .map_err(|_| AdapterStoreError::Conflict)?;
    RemoteExchangeRequest::new(
        open.lease.worker_id.clone(),
        open.lease.worker_instance_id.clone(),
        Vec::new(),
        frame.clone(),
    )
    .map_err(|_| AdapterStoreError::Conflict)?;
    let budget = MAX_REMOTE_FRAME_BYTES
        .checked_sub(frame.len())
        .ok_or(AdapterStoreError::Conflict)?
        / 4
        * 3;
    let budget = budget.min(RAW_CHUNK_BYTES);
    if budget == 0 {
        return Err(AdapterStoreError::Conflict);
    }
    Ok(budget)
}
fn check_remote_candidate_frame(
    message: &ExecutionPortMessage,
    open: &ArtifactOpenMessage,
) -> Result<(), AdapterStoreError> {
    use winwincode_execution_port::transport::{
        FrameDirection, RemoteExchangeRequest, RemoteTransportAdapter, TypedFrame,
    };
    let frame = TypedFrame::new(FrameDirection::WorkerToControlPlane, message.clone())
        .and_then(|frame| RemoteTransportAdapter::<CandidateNoopCore>::encode(&frame))
        .map_err(|_| AdapterStoreError::Conflict)?;
    let request = RemoteExchangeRequest::new(
        open.lease.worker_id.clone(),
        open.lease.worker_instance_id.clone(),
        Vec::new(),
        frame,
    )
    .map_err(|_| AdapterStoreError::Conflict)?;
    let bytes = request.encode().map_err(|_| AdapterStoreError::Conflict)?;
    RemoteExchangeRequest::decode(&bytes).map_err(|_| AdapterStoreError::Conflict)?;
    Ok(())
}
struct CandidateNoopCore;
impl winwincode_execution_port::transport::ExecutionPortCore for CandidateNoopCore {
    type Output = ();
    type Error = std::convert::Infallible;
    fn accept(&mut self, _: &ExecutionPortMessage) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn final_ack_matches_retry(retained: &ArtifactAckMessage, retry: &ArtifactAckMessage) -> bool {
    retained.retained_artifact == retry.retained_artifact
        && retained.ack_sequence == retry.ack_sequence
        && retained.artifact_id == retry.artifact_id
        && retained.error == retry.error
        && retained.kind == retry.kind
        && retained.lease == retry.lease
        && retained.message_id == retry.message_id
        && retained.replay_from_sequence == retry.replay_from_sequence
        && retained.schema_version == retry.schema_version
        && retained.sent_at == retry.sent_at
        && retained.session_identity == retry.session_identity
        && retained.worker_session_id == retry.worker_session_id
        && matches!(
            retained.status,
            LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
        )
        && matches!(
            retry.status,
            LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
        )
}

fn compact_prefix(
    transaction: &Transaction<'_>,
    record: &StoredCandidateArtifact,
    acknowledged: u64,
) -> Result<(), AdapterStoreError> {
    delete_delivery(transaction, &record.open_message.message_id.0)?;
    transaction.execute("DELETE FROM execution_outbox WHERE delivery_id IN (SELECT message_id FROM candidate_artifact_chunks WHERE authority_key=?1 AND sequence<=?2)", params![record.authority_key, i64::try_from(acknowledged).map_err(|_| AdapterStoreError::Corrupt)?]).map_err(|_| AdapterStoreError::Unavailable)?;
    Ok(())
}

fn queue_upgrade_chunks(
    transaction: &Transaction<'_>,
    record: &StoredCandidateArtifact,
) -> Result<Vec<DurableExecutionDelivery>, AdapterStoreError> {
    let mut replay = Vec::new();
    for sequence in 1..=record.chunk_count {
        let chunk = load_chunk(
            transaction,
            record,
            i64::try_from(sequence).map_err(|_| AdapterStoreError::Corrupt)?,
        )?
        .ok_or(AdapterStoreError::Corrupt)?;
        replay.push(ExecutionOutbox::retain_in_transaction(
            transaction,
            &ExecutionPortMessage::ArtifactChunkMessage(chunk),
        )?);
    }
    Ok(replay)
}

fn requeue_suffix(
    transaction: &Transaction<'_>,
    record: &StoredCandidateArtifact,
    replay_from: u64,
) -> Result<Vec<DurableExecutionDelivery>, AdapterStoreError> {
    let mut replay = Vec::new();
    for sequence in replay_from..=record.chunk_count {
        let chunk = load_chunk(
            transaction,
            record,
            i64::try_from(sequence).map_err(|_| AdapterStoreError::Corrupt)?,
        )?
        .ok_or(AdapterStoreError::Corrupt)?;
        {
            let changed = transaction
                .execute(
                    "UPDATE execution_outbox SET state = ?1 WHERE delivery_id = ?2",
                    params![PENDING, &chunk.message_id.0],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            if changed != 1 {
                return Err(AdapterStoreError::Corrupt);
            }
            replay.push(DurableExecutionDelivery {
                delivery_id: chunk.message_id.0.clone(),
                message: ExecutionPortMessage::ArtifactChunkMessage(chunk.clone()),
            });
        }
    }
    Ok(replay)
}

fn delete_delivery(
    transaction: &Transaction<'_>,
    delivery_id: &str,
) -> Result<(), AdapterStoreError> {
    transaction
        .execute(
            "DELETE FROM execution_outbox WHERE delivery_id = ?1",
            params![delivery_id],
        )
        .map_err(|_| AdapterStoreError::Unavailable)?;
    Ok(())
}

fn canonical_id(prefix: &str, namespace: &[u8], parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace);
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    let encoded = format!("{:x}", digest.finalize());
    format!("{prefix}_{}", &encoded[..26].to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use winwincode_domain::{
        ArtifactId, CodexThreadId, ExecutionAckSequence, ExecutionMessageId, FencingToken, LeaseId,
        RequestId, Revision, Sha256Digest, WorkContractId, WorkItemId, WorkRunId, WorkerInstanceId,
        WorkerSessionId,
    };
    use winwincode_execution_port::generated::{
        ArtifactAckMessageKind, ExecutionPortError, ExecutionPortErrorCode, WorkRunExecutionScope,
        WorkRunExecutionScopeKind,
    };

    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "winwincode-candidate-outbox-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    fn artifact_open_fixture() -> ArtifactOpenMessage {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .expect("execution fixture");
        let mut value = fixture["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find(|message| message["kind"] == "artifact.open")
            .expect("artifact.open")
            .clone();
        if let Some(scope) = value.pointer_mut("/scope") {
            *scope = serde_json::json!({
                "kind":"work-run", "productSessionId":"psn_00000000000000000000000001",
                "workContractId":"wct_00000000000000000000000001", "workContractRevision":2,
                "workItemId":"wit_00000000000000000000000001", "workItemRevision":1,
                "workRunId":"wrn_00000000000000000000000009", "attempt":1
            });
        }
        serde_json::from_value(value).expect("generated artifact.open")
    }

    fn upload(bytes: Vec<u8>) -> CandidateArtifactUpload {
        let mut open = artifact_open_fixture();
        let work_run_id = WorkRunId("wrn_00000000000000000000000009".to_owned());
        open.session_identity.work_run_id = Some(work_run_id.clone());
        CandidateArtifactUpload {
            snapshot_id: None,
            job_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(b"exact job"))),
            logical_job_digest: Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(b"logical job")
            )),
            execution_profile: "executor".into(),
            scope: ExecutionScope::WorkRunExecutionScope(WorkRunExecutionScope {
                attempt: 1,
                kind: WorkRunExecutionScopeKind::WorkRun,
                product_session_id: open.session_identity.product_session_id.clone(),
                rework_authorization: None,
                work_contract_id: WorkContractId("wct_00000000000000000000000001".to_owned()),
                work_contract_revision: Revision(2),
                work_item_id: WorkItemId("wit_00000000000000000000000001".to_owned()),
                work_item_revision: Revision(1),
                work_run_id,
            }),
            lease: open.lease,
            worker_session_id: open.worker_session_id,
            session_identity: open.session_identity,
            digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes))),
            bytes,
            created_at: open.sent_at,
            replacement_authority: None,
        }
    }

    fn acknowledgement(
        retained: &RetainedCandidateArtifact,
        upload: &CandidateArtifactUpload,
        sequence: i64,
        status: LeaseWriteStatus,
    ) -> ArtifactAckMessage {
        let gap = status == LeaseWriteStatus::Gap;
        ArtifactAckMessage {
            retained_artifact: None,
            ack_sequence: ExecutionAckSequence(sequence),
            artifact_id: retained.artifact.artifact_id.clone(),
            error: gap.then(|| ExecutionPortError {
                code: ExecutionPortErrorCode::SequenceGap,
                message: "candidate Artifact sequence gap".into(),
                retryable: true,
            }),
            kind: ArtifactAckMessageKind::ArtifactAck,
            lease: upload.lease.clone(),
            message_id: ExecutionMessageId(canonical_id(
                "xmsg",
                b"winwincode.candidate-artifact.test-ack.v1",
                &[
                    retained.artifact.artifact_id.0.as_bytes(),
                    &sequence.to_be_bytes(),
                ],
            )),
            replay_from_sequence: gap.then(|| ExecutionSequence(sequence.saturating_add(1))),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: upload.created_at.clone(),
            session_identity: upload.session_identity.clone(),
            status,
            worker_session_id: upload.worker_session_id.clone(),
        }
    }

    fn replacement_upload(predecessor: &CandidateArtifactUpload) -> CandidateArtifactUpload {
        let mut successor = predecessor.clone();
        successor.job_digest = Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(b"successor exact job")
        ));
        successor.lease.attempt = predecessor.lease.attempt.saturating_add(1);
        successor.lease.lease_id = LeaseId("lse_00000000000000000000000005".to_owned());
        successor.lease.fencing_token = FencingToken("43".to_owned());
        successor.lease.worker_instance_id =
            WorkerInstanceId("wki_00000000000000000000000003".to_owned());
        successor.worker_session_id = WorkerSessionId("wsn_00000000000000000000000006".to_owned());
        successor.session_identity.worker_session_id = successor.worker_session_id.clone();
        successor.session_identity.work_run_id =
            Some(WorkRunId("wrn_0000000000000000000000000A".to_owned()));
        successor.session_identity.codex_thread_id =
            CodexThreadId("cdx_0000000000000000000000000H".to_owned());
        if let ExecutionScope::WorkRunExecutionScope(scope) = &mut successor.scope {
            scope.attempt = successor.lease.attempt;
            scope.work_run_id = successor
                .session_identity
                .work_run_id
                .clone()
                .expect("WorkRun fixture");
        }
        successor.replacement_authority = Some(ExecutionJobReplacementAuthority {
            created_at: predecessor.created_at.clone(),
            logical_job_digest: Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(b"logical job")
            )),
            predecessor_lease: predecessor.lease.clone(),
            predecessor_session_identity: Some(predecessor.session_identity.clone()),
            receipt_digest: Sha256Digest(format!("sha256:{}", "e".repeat(64))),
            receipt_id: RequestId("req_00000000000000000000000010".to_owned()),
            scope: predecessor.scope.clone(),
            successor_lease: successor.lease.clone(),
        });
        successor
    }

    fn open_ledgers(
        root: &Path,
    ) -> Result<(CandidateArtifactOutbox, ExecutionOutbox), AdapterStoreError> {
        let store = AdapterStore::open(root)?;
        Ok((
            CandidateArtifactOutbox::open(store.clone())?,
            ExecutionOutbox::open(store)?,
        ))
    }

    fn persist_legacy_fixture(
        root: &Path,
        size: usize,
        acknowledged: u64,
        complete: bool,
        split: bool,
    ) -> (CandidateArtifactUpload, StoredCandidateArtifact) {
        let upload = upload(vec![0x7f; size]);
        let mut old = StoredCandidateArtifact::from_upload(&upload).unwrap();
        old.chunk_size_bytes = legacy_chunk_bytes();
        let template = old.chunk_messages[0].clone();
        old.chunk_messages = upload
            .bytes
            .chunks(legacy_chunk_bytes())
            .enumerate()
            .map(|(index, bytes)| {
                let sequence = (index + 1) as u64;
                let mut chunk = template.clone();
                chunk.sequence = ExecutionSequence(i64::try_from(sequence).unwrap());
                chunk.message_id = ExecutionMessageId(canonical_id(
                    "xmsg",
                    b"winwincode.candidate-artifact.chunk-message.v1",
                    &[
                        old.descriptor.artifact_id.0.as_bytes(),
                        &sequence.to_be_bytes(),
                    ],
                ));
                chunk.payload.data_base64 = BASE64_STANDARD.encode(bytes);
                chunk.payload.payload_digest =
                    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)));
                chunk.is_final = index + 1 == upload.bytes.chunks(legacy_chunk_bytes()).len();
                chunk
            })
            .collect();
        old.chunk_count = old.chunk_messages.len() as u64;
        old.ack_sequence = acknowledged;
        let retained = RetainedCandidateArtifact {
            artifact: old.reference(),
            authority: old.authority(),
            deliveries: Vec::new(),
            already_accepted: complete,
        };
        if complete {
            old.final_ack = Some(acknowledgement(
                &retained,
                &upload,
                i64::try_from(old.chunk_count).unwrap(),
                LeaseWriteStatus::Accepted,
            ));
        }
        old.validate().unwrap();
        let (candidate, execution) = open_ledgers(root).unwrap();
        candidate
            .store
            .transaction(|tx| {
                if split {
                    save_record(tx, &old)?;
                } else {
                    let mut legacy = serde_json::to_value(&old).unwrap();
                    legacy["bytes"] = serde_json::json!(old.bytes);
                    legacy["chunk_messages"] = serde_json::json!(old.chunk_messages);
                    legacy.as_object_mut().unwrap().remove("chunk_count");
                    legacy.as_object_mut().unwrap().remove("chunk_size_bytes");
                    tx.execute(
                        "INSERT INTO candidate_artifact_upload VALUES(?1,?2,?3)",
                        params![
                            old.authority_key,
                            old.descriptor.artifact_id.0,
                            serde_json::to_vec(&legacy).unwrap()
                        ],
                    )
                    .unwrap();
                }
                if !complete {
                    ExecutionOutbox::retain_in_transaction(
                        tx,
                        &ExecutionPortMessage::ArtifactOpenMessage(old.open_message.clone()),
                    )?;
                    for chunk in old
                        .chunk_messages
                        .iter()
                        .filter(|chunk| chunk.sequence.0 > i64::try_from(acknowledged).unwrap())
                    {
                        ExecutionOutbox::retain_in_transaction(
                            tx,
                            &ExecutionPortMessage::ArtifactChunkMessage(chunk.clone()),
                        )?;
                    }
                }
                Ok(())
            })
            .unwrap();
        drop(execution);
        drop(candidate);
        (upload, old)
    }

    #[test]
    fn legacy_sqlite_plans_upgrade_atomically_and_preserve_completed_candidates() {
        for (size, acknowledged, complete, split) in [
            (100, 0, false, false),
            (3 * 1024 * 1024 + 17000, 0, false, false),
            (3 * 1024 * 1024 + 17000, 1, false, false),
            (3 * 1024 * 1024 + 17000, 1, false, true),
            (3 * 1024 * 1024 + 17000, 2, true, false),
        ] {
            let root = test_root("legacy-upgrade");
            let (upload, old) = persist_legacy_fixture(&root, size, acknowledged, complete, split);
            let (candidate, execution) = open_ledgers(&root).unwrap();
            let retained = candidate.retain(&upload).unwrap();
            if size == 100 || complete {
                assert_eq!(retained.artifact, old.reference());
                assert_eq!(retained.already_accepted, complete);
                if complete {
                    assert!(execution.pending().unwrap().is_empty());
                }
            } else {
                assert_ne!(retained.artifact.artifact_id, old.descriptor.artifact_id);
                assert_eq!(retained.artifact.digest, old.descriptor.digest);
                let pending = execution.pending().unwrap();
                assert_eq!(
                    pending.len(),
                    1,
                    "only the replacement open may precede its handshake"
                );
                let ExecutionPortMessage::ArtifactOpenMessage(open) = &pending[0].message else {
                    panic!("replacement open")
                };
                assert_eq!(
                    open.replaces_artifact_id,
                    Some(old.descriptor.artifact_id.clone())
                );
                check_remote_candidate_frame(&pending[0].message, open).unwrap();
                let late = acknowledgement(
                    &RetainedCandidateArtifact {
                        artifact: old.reference(),
                        authority: old.authority(),
                        deliveries: Vec::new(),
                        already_accepted: false,
                    },
                    &upload,
                    i64::try_from(acknowledged).unwrap(),
                    LeaseWriteStatus::Accepted,
                );
                assert_eq!(
                    candidate.apply_ack(&late).unwrap(),
                    CandidateArtifactAckOutcome::Pending
                );
                let mut ack = acknowledgement(&retained, &upload, 0, LeaseWriteStatus::Accepted);
                ack.message_id = open.message_id.clone();
                let CandidateArtifactAckOutcome::Replay(chunks) =
                    candidate.apply_ack(&ack).unwrap()
                else {
                    panic!("bounded plan is released after handshake")
                };
                assert!(chunks.len() > 48);
                for delivery in &chunks {
                    check_remote_candidate_frame(&delivery.message, open).unwrap();
                    assert!(candidate.delivery_allowed(&delivery.message).unwrap());
                }
                assert!(
                    !candidate
                        .delivery_allowed(&ExecutionPortMessage::ArtifactChunkMessage(
                            old.chunk_messages[0].clone()
                        ))
                        .unwrap()
                );
                drop(candidate);
                drop(execution);
                let (candidate, execution) = open_ledgers(&root).unwrap();
                assert_eq!(
                    candidate.retain(&upload).unwrap().artifact,
                    retained.artifact
                );
                assert_eq!(execution.pending().unwrap().len(), chunks.len());
                let final_ack = acknowledgement(
                    &retained,
                    &upload,
                    i64::try_from(chunks.len()).unwrap(),
                    LeaseWriteStatus::Accepted,
                );
                assert_eq!(
                    candidate.apply_ack(&final_ack).unwrap(),
                    CandidateArtifactAckOutcome::Accepted(retained.artifact)
                );
                assert!(execution.pending().unwrap().is_empty());
                drop(candidate);
                drop(execution);
                std::fs::remove_dir_all(root).unwrap();
                continue;
            }
            drop(candidate);
            drop(execution);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn interrupted_transport_upgrade_keeps_the_old_sqlite_and_outbox_until_commit() {
        let root = test_root("upgrade-rollback");
        let (_, old) = persist_legacy_fixture(&root, 300 * 1024, 0, false, false);
        let store = AdapterStore::open(&root).unwrap();
        store.lock().unwrap().execute_batch("CREATE TRIGGER fail_upgrade BEFORE INSERT ON candidate_artifact_retired BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        assert!(CandidateArtifactOutbox::open(store.clone()).is_err());
        let execution = ExecutionOutbox::open(store.clone()).unwrap();
        assert_eq!(execution.pending().unwrap().len(), 2);
        assert_eq!(
            load_by_authority_connection(&store.lock().unwrap(), &old.authority_key)
                .unwrap()
                .unwrap()
                .descriptor
                .artifact_id,
            old.descriptor.artifact_id
        );
        store
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_upgrade")
            .unwrap();
        let candidate = CandidateArtifactOutbox::open(store.clone()).unwrap();
        let first = execution.pending().unwrap();
        assert_eq!(first.len(), 1);
        drop(candidate);
        drop(execution);
        drop(store);
        let (candidate, execution) = open_ledgers(&root).unwrap();
        assert!(
            execution.pending().unwrap() == first,
            "restart before handshake preserves the replacement identity"
        );
        drop(candidate);
        drop(execution);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upgraded_open_retains_a_server_completed_predecessor_without_sending_chunks() {
        let root = test_root("legacy-server-complete");
        let (upload, old) = persist_legacy_fixture(&root, 300 * 1024, 0, false, true);
        let (candidate, execution) = open_ledgers(&root).unwrap();
        let retained = candidate.retain(&upload).unwrap();
        let pending = execution.pending().unwrap();
        let ExecutionPortMessage::ArtifactOpenMessage(open) = &pending[0].message else {
            panic!("open")
        };
        let mut ack = acknowledgement(&retained, &upload, 0, LeaseWriteStatus::Duplicate);
        ack.message_id = open.message_id.clone();
        ack.retained_artifact = Some(old.reference());
        let mut forged = ack.clone();
        forged.retained_artifact.as_mut().unwrap().digest =
            Sha256Digest(format!("sha256:{}", "0".repeat(64)));
        assert!(candidate.apply_ack(&forged).is_err());
        assert_eq!(
            candidate.apply_ack(&ack).unwrap(),
            CandidateArtifactAckOutcome::Accepted(old.reference())
        );
        assert!(execution.pending().unwrap().is_empty());
        drop(candidate);
        drop(execution);
        let (candidate, execution) = open_ledgers(&root).unwrap();
        assert_eq!(
            candidate.accepted_reference(&upload.authority()).unwrap(),
            Some(old.reference())
        );
        assert_eq!(
            candidate.apply_ack(&ack).unwrap(),
            CandidateArtifactAckOutcome::Accepted(old.reference())
        );
        assert!(execution.pending().unwrap().is_empty());
        drop(candidate);
        drop(execution);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn large_candidate_frames_round_trip_and_ack_writes_only_small_metadata() {
        let root = test_root("wire-budget-immutable-acks");
        let upload = upload(vec![0x7f; 4 * 1024 * 1024 + 1]);
        let (candidate, execution) = open_ledgers(&root).unwrap();
        let retained = candidate.retain(&upload).unwrap();
        assert!(retained.deliveries.len() > 32);
        for delivery in &retained.deliveries {
            let ExecutionPortMessage::ArtifactOpenMessage(open) = &retained.deliveries[0].message
            else {
                panic!("candidate open")
            };
            check_remote_candidate_frame(&delivery.message, open).unwrap();
            assert!(candidate.delivery_allowed(&delivery.message).unwrap());
        }
        let connection = candidate.store.lock().unwrap();
        let metadata: Vec<u8> = connection
            .query_row(
                "SELECT record_json FROM candidate_artifact_upload",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(metadata.len() < 16 * 1024);
        connection.execute_batch("CREATE TRIGGER immutable_content BEFORE UPDATE ON candidate_artifact_content BEGIN SELECT RAISE(ABORT,'immutable content'); END;
            CREATE TRIGGER immutable_chunks BEFORE UPDATE ON candidate_artifact_chunks BEGIN SELECT RAISE(ABORT,'immutable chunks'); END;
            CREATE TRIGGER no_reinsert_content BEFORE INSERT ON candidate_artifact_content BEGIN SELECT RAISE(ABORT,'content already retained'); END;
            CREATE TRIGGER no_reinsert_chunks BEFORE INSERT ON candidate_artifact_chunks BEGIN SELECT RAISE(ABORT,'chunks already retained'); END;").unwrap();
        drop(connection);
        for sequence in 0..i64::try_from(retained.deliveries.len()).unwrap() {
            candidate
                .apply_ack(&acknowledgement(
                    &retained,
                    &upload,
                    sequence,
                    LeaseWriteStatus::Accepted,
                ))
                .unwrap();
        }
        assert!(
            candidate
                .accepted_reference(&upload.authority())
                .unwrap()
                .is_some()
        );
        assert!(execution.pending().unwrap().is_empty());
        drop(candidate);
        drop(execution);
        let (candidate, execution) = open_ledgers(&root).unwrap();
        assert!(
            candidate
                .accepted_reference(&upload.authority())
                .unwrap()
                .is_some()
        );
        assert!(execution.pending().unwrap().is_empty());
        drop(candidate);
        drop(execution);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_without_atomic_retention_leaves_no_candidate_product() {
        let root = test_root("snapshot-only");
        let (candidate, execution) = open_ledgers(&root).expect("open ledgers");
        assert!(execution.pending().expect("pending").is_empty());
        let upload = upload(b"candidate".to_vec());
        assert_eq!(
            candidate
                .accepted_reference(&upload.authority())
                .expect("no accepted reference"),
            None
        );
        drop(candidate);
        drop(execution);
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn retain_send_loss_and_restart_preserve_exact_candidate_bytes_and_identities() {
        for chat in [false, true] {
            let root = test_root("retain-restart");
            let mut upload = upload(br#"{"candidate":"exact"}"#.to_vec());
            if chat {
                upload.execution_profile = "codex-chat".into();
                upload.session_identity.work_run_id = None;
                upload.scope = serde_json::from_value(serde_json::json!({
                "kind":"product-session", "productSessionId": upload.session_identity.product_session_id,
            })).expect("Chat scope");
            }
            let original;
            let retained;
            {
                let (candidate, execution) = open_ledgers(&root).expect("open ledgers");
                retained = candidate.retain(&upload).expect("retain candidate");
                assert_eq!(retained.deliveries.len(), 2);
                for delivery in &retained.deliveries {
                    assert!(
                        candidate
                            .delivery_allowed(&delivery.message)
                            .expect("retained frame is eligible")
                    );
                }
                let ExecutionPortMessage::ArtifactOpenMessage(open) =
                    &retained.deliveries[0].message
                else {
                    panic!("artifact open")
                };
                assert_eq!(
                    open.artifact.media_type,
                    if chat {
                        "application/zip"
                    } else {
                        CANDIDATE_MEDIA_TYPE
                    }
                );
                assert!(!retained.already_accepted);
                original = retained.deliveries.clone();
                for delivery in &retained.deliveries {
                    execution
                        .record_sent(&delivery.delivery_id)
                        .expect("record sent attempt");
                }
                assert_eq!(execution.pending().expect("response-loss retry"), original);
            }
            {
                let (candidate, execution) = open_ledgers(&root).expect("restart ledgers");
                assert_eq!(execution.pending().expect("restart retry"), original);
                let replay = candidate.retain(&upload).expect("exact retain replay");
                assert_eq!(replay.artifact, retained.artifact);
                assert!(replay.deliveries.is_empty());
                assert!(!replay.already_accepted);

                let mut changed_bytes = upload.clone();
                changed_bytes.bytes.push(b'!');
                changed_bytes.digest =
                    Sha256Digest(format!("sha256:{:x}", Sha256::digest(&changed_bytes.bytes)));
                assert_eq!(
                    candidate.retain(&changed_bytes),
                    Err(AdapterStoreError::Conflict)
                );
                let mut changed_job = upload.clone();
                changed_job.job_digest =
                    Sha256Digest(format!("sha256:{:x}", Sha256::digest(b"foreign job")));
                assert_eq!(
                    candidate.retain(&changed_job),
                    Err(AdapterStoreError::Conflict)
                );
                let mut changed_role = upload.clone();
                changed_role.execution_profile = "remediator".into();
                assert_eq!(
                    candidate.retain(&changed_role),
                    Err(AdapterStoreError::Conflict)
                );
            }
            std::fs::remove_dir_all(root).expect("remove fixture");
        }
    }

    #[test]
    fn gap_final_ack_and_restart_gate_one_exact_candidate_reference() {
        let root = test_root("ack-restart");
        let upload = upload(vec![b'x'; RAW_CHUNK_BYTES + 1]);
        let (candidate, execution) = open_ledgers(&root).expect("open ledgers");
        let retained = candidate.retain(&upload).expect("retain candidate");
        assert_eq!(retained.deliveries.len(), 3);

        let mut wrong = acknowledgement(&retained, &upload, 0, LeaseWriteStatus::Accepted);
        wrong.artifact_id = ArtifactId(canonical_id(
            "art",
            b"winwincode.candidate-artifact.foreign.v1",
            &[b"foreign"],
        ));
        assert_eq!(
            candidate.apply_ack(&wrong),
            Err(AdapterStoreError::Conflict)
        );
        assert_eq!(execution.pending().expect("unchanged pending").len(), 3);

        let gap = acknowledgement(&retained, &upload, 0, LeaseWriteStatus::Gap);
        let CandidateArtifactAckOutcome::Replay(replay) =
            candidate.apply_ack(&gap).expect("replay suffix")
        else {
            panic!("gap must replay original suffix")
        };
        assert_eq!(replay, retained.deliveries[1..]);
        assert_eq!(execution.pending().expect("chunk suffix"), replay);

        let first = acknowledgement(&retained, &upload, 1, LeaseWriteStatus::Accepted);
        assert_eq!(
            candidate.apply_ack(&first).expect("ack first chunk"),
            CandidateArtifactAckOutcome::Pending
        );
        assert_eq!(execution.pending().expect("final chunk only").len(), 1);
        drop(candidate);
        drop(execution);

        let (candidate, execution) = open_ledgers(&root).expect("restart before final ack");
        assert_eq!(execution.pending().expect("restart final chunk").len(), 1);
        let final_ack = acknowledgement(&retained, &upload, 2, LeaseWriteStatus::Accepted);
        assert_eq!(
            candidate.apply_ack(&final_ack).expect("final ack"),
            CandidateArtifactAckOutcome::Accepted(retained.artifact.clone())
        );
        assert!(execution.pending().expect("all compacted").is_empty());
        drop(candidate);
        drop(execution);

        let (candidate, execution) = open_ledgers(&root).expect("restart after final ack");
        assert!(execution.pending().expect("no duplicate upload").is_empty());
        assert_eq!(
            candidate
                .accepted_reference(&upload.authority())
                .expect("accepted reference"),
            Some(retained.artifact.clone())
        );
        assert_eq!(
            candidate.apply_ack(&final_ack).expect("exact ack replay"),
            CandidateArtifactAckOutcome::Accepted(retained.artifact.clone())
        );
        let stale_open = acknowledgement(&retained, &upload, 0, LeaseWriteStatus::Duplicate);
        assert_eq!(
            candidate.apply_ack(&stale_open),
            Err(AdapterStoreError::Conflict)
        );
        let mut foreign_job = upload.job_digest.clone();
        foreign_job.0 = format!("sha256:{:x}", Sha256::digest(b"foreign job"));
        let mut foreign_authority = upload.authority();
        foreign_authority.job_digest = foreign_job;
        assert_eq!(
            candidate.accepted_reference(&foreign_authority),
            Err(AdapterStoreError::Conflict)
        );
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn final_ack_retry_accepts_control_plane_duplicate_status() {
        let root = test_root("final-ack-duplicate");
        let upload = upload(br#"{"candidate":"duplicate"}"#.to_vec());
        let (candidate, execution) = open_ledgers(&root).expect("open ledgers");
        let retained = candidate.retain(&upload).expect("retain candidate");
        let accepted = acknowledgement(&retained, &upload, 1, LeaseWriteStatus::Accepted);
        assert_eq!(
            candidate.apply_ack(&accepted).expect("accept final ack"),
            CandidateArtifactAckOutcome::Accepted(retained.artifact.clone())
        );
        let mut duplicate = accepted.clone();
        duplicate.status = LeaseWriteStatus::Duplicate;
        assert_eq!(
            candidate
                .apply_ack(&duplicate)
                .expect("replay duplicate final ack"),
            CandidateArtifactAckOutcome::Accepted(retained.artifact)
        );
        assert!(execution.pending().expect("compact upload").is_empty());
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn cancellation_intent_survives_restart_and_blocks_candidate_replay() {
        let root = test_root("cancel-intent-restart");
        let upload = upload(br#"{"candidate":"cancel-intent"}"#.to_vec());
        let retained;
        {
            let (candidate, execution) = open_ledgers(&root).expect("open ledgers");
            retained = candidate.retain(&upload).expect("retain candidate");
            for delivery in &retained.deliveries {
                execution
                    .record_sent(&delivery.delivery_id)
                    .expect("record attempted upload");
            }
            candidate
                .request_cancel(&upload.authority())
                .expect("commit cancel intent");
            assert!(
                !candidate
                    .delivery_allowed(&retained.deliveries[0].message)
                    .expect("open delivery gate")
            );
            assert_eq!(
                candidate.apply_ack(&acknowledgement(
                    &retained,
                    &upload,
                    1,
                    LeaseWriteStatus::Accepted,
                )),
                Ok(CandidateArtifactAckOutcome::Pending)
            );
            assert_eq!(
                candidate.retain(&upload),
                Err(AdapterStoreError::Conflict),
                "a committed cancellation intent must not be revived by retain"
            );
        }
        {
            let (candidate, execution) = open_ledgers(&root).expect("restart ledgers");
            assert_eq!(
                execution
                    .pending()
                    .expect("pending frames survive intent")
                    .len(),
                2
            );
            assert!(
                !candidate
                    .delivery_allowed(&retained.deliveries[1].message)
                    .expect("chunk delivery gate")
            );
            candidate
                .cancel(&upload.authority())
                .expect("retry cancel cleanup");
            assert!(execution.pending().expect("cancelled frames").is_empty());
            assert!(
                !candidate
                    .delivery_allowed(&retained.deliveries[0].message)
                    .expect("deleted delivery gate")
            );
            assert_eq!(candidate.retain(&upload), Err(AdapterStoreError::Conflict));
            assert_eq!(
                candidate.apply_ack(&acknowledgement(
                    &retained,
                    &upload,
                    1,
                    LeaseWriteStatus::Accepted,
                )),
                Ok(CandidateArtifactAckOutcome::Pending),
                "an exact delayed receipt is consumed without reviving the upload"
            );
            assert!(execution.pending().unwrap().is_empty());
        }
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn replacement_authority_cancels_a_retained_predecessor_after_restart() {
        let root = test_root("replacement-cancel");
        let predecessor = upload(br#"{"candidate":"replacement-cancel"}"#.to_vec());
        let (candidate, execution) = open_ledgers(&root).expect("open ledgers");
        let retained = candidate
            .retain(&predecessor)
            .expect("retain predecessor candidate");
        let successor = replacement_upload(&predecessor);
        candidate
            .request_cancel(&successor.authority())
            .expect("commit successor cancellation intent");
        assert!(
            !candidate
                .delivery_allowed(&retained.deliveries[0].message)
                .expect("predecessor delivery gate")
        );
        candidate
            .cancel(&successor.authority())
            .expect("clean predecessor using successor authority");
        assert!(execution.pending().expect("cancelled upload").is_empty());
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn sealed_replacement_reuses_the_predecessor_artifact_and_original_frames() {
        let root = test_root("replacement-stream");
        let predecessor = upload(br#"{"candidate":"replacement"}"#.to_vec());
        let (candidate, execution) = open_ledgers(&root).expect("open ledgers");
        let original = candidate
            .retain(&predecessor)
            .expect("retain predecessor candidate");
        assert_eq!(original.authority, predecessor.authority());
        assert_eq!(original.deliveries.len(), 2);
        for delivery in &original.deliveries {
            execution
                .record_sent(&delivery.delivery_id)
                .expect("record predecessor send");
        }

        let successor = replacement_upload(&predecessor);
        let resumed = candidate
            .retain(&successor)
            .expect("resume sealed predecessor stream");
        assert_eq!(resumed.artifact, original.artifact);
        assert_eq!(resumed.authority, predecessor.authority());
        assert!(resumed.deliveries.is_empty());
        assert!(!resumed.already_accepted);
        assert_eq!(
            execution.pending().expect("original pending frames"),
            original.deliveries
        );

        let mut changed = successor.clone();
        changed.bytes.push(b'!');
        changed.digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&changed.bytes)));
        assert_eq!(candidate.retain(&changed), Err(AdapterStoreError::Conflict));

        let final_ack = acknowledgement(&original, &predecessor, 1, LeaseWriteStatus::Accepted);
        assert_eq!(
            candidate
                .apply_ack(&final_ack)
                .expect("accept original stream"),
            CandidateArtifactAckOutcome::Accepted(original.artifact.clone())
        );
        let accepted = candidate
            .retain(&successor)
            .expect("successor recovers accepted predecessor reference");
        assert_eq!(accepted.artifact, original.artifact);
        assert_eq!(accepted.authority, predecessor.authority());
        assert!(accepted.already_accepted);
        assert_eq!(
            candidate
                .accepted_reference(&predecessor.authority())
                .expect("predecessor accepted reference"),
            Some(original.artifact.clone())
        );
        assert_eq!(
            candidate
                .accepted_reference(&successor.authority())
                .expect("successor accepted reference stays exact"),
            None
        );
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn retired_upload_consumes_negative_receipts_without_advancing_its_successor() {
        let root = test_root("retired-negative-ack");
        let (upload, old) = persist_legacy_fixture(&root, 300 * 1024, 0, false, false);
        let (candidate, execution) = open_ledgers(&root).unwrap();
        let retained = candidate.retain(&upload).unwrap();
        assert_ne!(retained.artifact.artifact_id, old.descriptor.artifact_id);
        let late = acknowledgement(
            &RetainedCandidateArtifact {
                artifact: old.reference(),
                authority: old.authority(),
                deliveries: vec![],
                already_accepted: false,
            },
            &upload,
            0,
            LeaseWriteStatus::Accepted,
        );
        for (status, code) in [
            (
                LeaseWriteStatus::RejectedExpiredLease,
                ExecutionPortErrorCode::LeaseExpired,
            ),
            (
                LeaseWriteStatus::RejectedStaleFencingToken,
                ExecutionPortErrorCode::StaleFencingToken,
            ),
            (
                LeaseWriteStatus::RejectedWorkerInstance,
                ExecutionPortErrorCode::WorkerInstanceChanged,
            ),
        ] {
            let mut rejected = late.clone();
            rejected.status = status;
            rejected.error = Some(ExecutionPortError {
                code,
                message: "retired upload rejected".into(),
                retryable: false,
            });
            assert_eq!(
                candidate.apply_ack(&rejected).unwrap(),
                CandidateArtifactAckOutcome::Pending
            );
            assert_eq!(
                execution.pending().unwrap().len(),
                1,
                "negative retired ACK cannot release successor content"
            );
            assert_eq!(
                candidate.accepted_reference(&upload.authority()).unwrap(),
                None
            );
        }
        drop(candidate);
        drop(execution);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancelled_upload_consumes_exact_negative_receipts_and_rejects_bad_shapes() {
        let root = test_root("cancelled-negative-ack");
        let upload = upload(b"candidate".to_vec());
        let (candidate, execution) = open_ledgers(&root).unwrap();
        let retained = candidate.retain(&upload).unwrap();
        let negative = [
            (
                LeaseWriteStatus::RejectedExpiredLease,
                ExecutionPortErrorCode::LeaseExpired,
            ),
            (
                LeaseWriteStatus::RejectedStaleFencingToken,
                ExecutionPortErrorCode::StaleFencingToken,
            ),
            (
                LeaseWriteStatus::RejectedWorkerInstance,
                ExecutionPortErrorCode::WorkerInstanceChanged,
            ),
        ]
        .map(|(status, code)| {
            let mut ack = acknowledgement(&retained, &upload, 0, status);
            ack.error = Some(ExecutionPortError {
                code,
                message: "old upload rejected".into(),
                retryable: false,
            });
            ack
        });
        for ack in &negative {
            assert_eq!(candidate.apply_ack(ack), Err(AdapterStoreError::Conflict));
        }
        candidate.cancel(&upload.authority()).unwrap();
        drop(candidate);
        drop(execution);
        let (candidate, execution) = open_ledgers(&root).unwrap();
        for ack in negative {
            assert_eq!(
                candidate.apply_ack(&ack),
                Ok(CandidateArtifactAckOutcome::Pending)
            );
            for field in 0..7 {
                let mut bad = ack.clone();
                match field {
                    0 => bad.error = None,
                    1 => bad.error.as_mut().unwrap().code = ExecutionPortErrorCode::MessageConflict,
                    2 => bad.error.as_mut().unwrap().retryable = true,
                    3 => bad.lease.fencing_token = FencingToken("foreign".into()),
                    4 => bad.ack_sequence.0 = 999,
                    5 => bad.replay_from_sequence = Some(ExecutionSequence(1)),
                    _ => bad.retained_artifact = Some(retained.artifact.clone()),
                }
                assert_eq!(candidate.apply_ack(&bad), Err(AdapterStoreError::Conflict));
            }
        }
        assert!(execution.pending().unwrap().is_empty());
        assert_eq!(
            candidate.accepted_reference(&upload.authority()).unwrap(),
            None
        );
        assert_eq!(candidate.retain(&upload), Err(AdapterStoreError::Conflict));
        drop(candidate);
        drop(execution);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancel_removes_unaccepted_upload_and_cannot_remove_an_accepted_candidate() {
        let root = test_root("cancel");
        let upload = upload(br#"{"candidate":"cancelled"}"#.to_vec());
        let (candidate, execution) = open_ledgers(&root).expect("open ledgers");
        let first = candidate.retain(&upload).expect("retain candidate");
        assert_eq!(execution.pending().expect("pending upload").len(), 2);

        candidate
            .cancel(&upload.authority())
            .expect("cancel pending upload");
        assert!(execution.pending().expect("cancelled outbox").is_empty());
        assert_eq!(
            candidate
                .accepted_reference(&upload.authority())
                .expect("cancelled reference"),
            None
        );
        candidate
            .cancel(&upload.authority())
            .expect("repeated cancel is exact");

        let late = acknowledgement(&first, &upload, 1, LeaseWriteStatus::Accepted);
        assert_eq!(
            candidate
                .apply_ack(&late)
                .expect("exact cancelled-stream ACK is consumed"),
            CandidateArtifactAckOutcome::Pending
        );
        assert!(execution.pending().unwrap().is_empty());

        assert_eq!(candidate.retain(&upload), Err(AdapterStoreError::Conflict));
        let mut foreign = late.clone();
        foreign.lease.fencing_token = FencingToken("999".into());
        assert_eq!(
            candidate.apply_ack(&foreign),
            Err(AdapterStoreError::Conflict)
        );
        let mut invalid = late.clone();
        invalid.ack_sequence.0 = 999;
        assert_eq!(
            candidate.apply_ack(&invalid),
            Err(AdapterStoreError::Conflict)
        );
        drop(candidate);
        drop(execution);
        let (candidate, execution) = open_ledgers(&root).unwrap();
        assert_eq!(
            candidate.apply_ack(&late),
            Ok(CandidateArtifactAckOutcome::Pending)
        );
        assert_eq!(
            candidate.accepted_reference(&upload.authority()).unwrap(),
            None
        );
        assert!(execution.pending().unwrap().is_empty());
        let mut next_attempt = upload.clone();
        next_attempt.lease.fencing_token = FencingToken("2".into());
        let retained = candidate
            .retain(&next_attempt)
            .expect("fresh fenced attempt");
        assert_ne!(retained.artifact, first.artifact);
        let final_ack = acknowledgement(&retained, &next_attempt, 1, LeaseWriteStatus::Accepted);
        assert_eq!(
            candidate.apply_ack(&final_ack).expect("accept candidate"),
            CandidateArtifactAckOutcome::Accepted(retained.artifact.clone())
        );
        assert_eq!(
            candidate.cancel(&next_attempt.authority()),
            Err(AdapterStoreError::Conflict)
        );
        assert_eq!(
            candidate
                .accepted_reference(&next_attempt.authority())
                .expect("accepted reference survives cancel"),
            Some(retained.artifact)
        );
        std::fs::remove_dir_all(root).expect("remove fixture");
    }
}
