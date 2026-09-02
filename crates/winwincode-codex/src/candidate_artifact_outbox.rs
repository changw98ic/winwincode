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
    ExecutionJobReplacementAuthority, ExecutionLeaseStamp, ExecutionPortMessage, ExecutionScope,
    LeaseWriteStatus,
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

// Leaves ample room below EncodedPayload's base64 ceiling.
const RAW_CHUNK_BYTES: usize = 3 * 1024 * 1024;
const PENDING: &str = "pending";

/// Exact verified bytes and execution authority entering the durable ledger.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateArtifactUpload {
    pub job_digest: Sha256Digest,
    pub logical_job_digest: Sha256Digest,
    pub execution_profile: String,
    pub scope: ExecutionScope,
    pub lease: ExecutionLeaseStamp,
    pub worker_session_id: WorkerSessionId,
    pub session_identity: winwincode_domain::SessionIdentity,
    pub bytes: Vec<u8>,
    pub digest: Sha256Digest,
    pub created_at: Instant,
    pub replacement_authority: Option<ExecutionJobReplacementAuthority>,
}

impl CandidateArtifactUpload {
    /// Returns the immutable Job/lease/session identity without copying bytes.
    #[must_use]
    pub fn authority(&self) -> CandidateArtifactAuthority {
        CandidateArtifactAuthority {
            job_digest: self.job_digest.clone(),
            logical_job_digest: self.logical_job_digest.clone(),
            execution_profile: self.execution_profile.clone(),
            scope: self.scope.clone(),
            lease: self.lease.clone(),
            worker_session_id: self.worker_session_id.clone(),
            session_identity: self.session_identity.clone(),
        }
    }
}

/// Exact candidate upload authority used to recover a final accepted reference.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateArtifactAuthority {
    pub job_digest: Sha256Digest,
    pub logical_job_digest: Sha256Digest,
    pub execution_profile: String,
    pub scope: ExecutionScope,
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
                 );",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(Self { store })
    }

    pub(crate) fn retain(
        &self,
        upload: &CandidateArtifactUpload,
    ) -> Result<RetainedCandidateArtifact, AdapterStoreError> {
        let record = StoredCandidateArtifact::from_upload(upload)?;
        self.store.transaction(|transaction| {
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
            if let Some(existing) = replacement_record(transaction, upload)? {
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
            let mut record = load_by_artifact(transaction, &acknowledgement.artifact_id)?
                .ok_or(AdapterStoreError::Conflict)?;
            record.validate()?;
            record.validate_ack_authority(acknowledgement)?;
            if record.cancel_requested {
                return Err(AdapterStoreError::Conflict);
            }
            if let Some(final_ack) = &record.final_ack {
                return if final_ack_matches_retry(final_ack, acknowledgement) {
                    Ok(CandidateArtifactAckOutcome::Accepted(record.reference()))
                } else {
                    Err(AdapterStoreError::Conflict)
                };
            }
            let acknowledged = u64::try_from(acknowledgement.ack_sequence.0)
                .map_err(|_| AdapterStoreError::Conflict)?;
            let final_sequence = record.final_sequence()?;
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
                    compact_prefix(transaction, &record, acknowledged)?;
                    record.ack_sequence = acknowledged;
                    if acknowledged == final_sequence {
                        record.final_ack = Some(acknowledgement.clone());
                        save_record(transaction, &record)?;
                        Ok(CandidateArtifactAckOutcome::Accepted(record.reference()))
                    } else {
                        save_record(transaction, &record)?;
                        Ok(CandidateArtifactAckOutcome::Pending)
                    }
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
                || record.scope != authority.scope
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
                    && open.artifact.media_type == CANDIDATE_MEDIA_TYPE =>
            {
                &open.artifact.artifact_id
            }
            ExecutionPortMessage::ArtifactChunkMessage(chunk)
                if chunk.payload.content_type == CANDIDATE_MEDIA_TYPE =>
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
            ExecutionPortMessage::ArtifactChunkMessage(chunk) => record
                .chunk_messages
                .iter()
                .any(|retained| retained == chunk),
            _ => false,
        };
        if !exact_frame {
            return Err(AdapterStoreError::Conflict);
        }
        Ok(!record.cancel_requested && record.final_ack.is_none())
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
                || record.scope != authority.scope
                || record.final_ack.is_some()
            {
                return Err(AdapterStoreError::Conflict);
            }
            delete_delivery(transaction, &record.open_message.message_id.0)?;
            for chunk in &record.chunk_messages {
                delete_delivery(transaction, &chunk.message_id.0)?;
            }
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
    bytes: Vec<u8>,
    descriptor: ArtifactDescriptor,
    open_message: ArtifactOpenMessage,
    chunk_messages: Vec<ArtifactChunkMessage>,
    ack_sequence: u64,
    final_ack: Option<ArtifactAckMessage>,
    #[serde(default)]
    cancel_requested: bool,
    #[serde(default)]
    replacement_authority: Option<ExecutionJobReplacementAuthority>,
}

impl StoredCandidateArtifact {
    fn from_upload(upload: &CandidateArtifactUpload) -> Result<Self, AdapterStoreError> {
        validate_upload(upload)?;
        if let Some(replacement) = upload.replacement_authority.as_ref() {
            validate_replacement_upload(upload, replacement)?;
        }
        let authority_key = authority_key(
            &upload.lease,
            &upload.worker_session_id,
            &upload.session_identity,
        )?;
        let artifact_id = ArtifactId(canonical_id(
            "art",
            b"winwincode.candidate-artifact.v1",
            &[authority_key.as_bytes(), upload.digest.0.as_bytes()],
        ));
        let size_bytes =
            i64::try_from(upload.bytes.len()).map_err(|_| AdapterStoreError::Conflict)?;
        let descriptor = ArtifactDescriptor {
            artifact_id: artifact_id.clone(),
            digest: upload.digest.clone(),
            file_name: Some(CANDIDATE_FILE_NAME.to_owned()),
            kind: ArtifactKind::Candidate,
            media_type: CANDIDATE_MEDIA_TYPE.to_owned(),
            size_bytes,
        };
        let open_message = ArtifactOpenMessage {
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
            worker_session_id: upload.worker_session_id.clone(),
        };
        let chunk_messages = upload
            .bytes
            .chunks(RAW_CHUNK_BYTES)
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
                    is_final: index + 1 == upload.bytes.chunks(RAW_CHUNK_BYTES).len(),
                    kind: ArtifactChunkMessageKind::ArtifactChunk,
                    lease: upload.lease.clone(),
                    message_id: ExecutionMessageId(canonical_id(
                        "xmsg",
                        b"winwincode.candidate-artifact.chunk-message.v1",
                        &[artifact_id.0.as_bytes(), &sequence.to_be_bytes()],
                    )),
                    payload: EncodedPayload {
                        content_type: CANDIDATE_MEDIA_TYPE.to_owned(),
                        data_base64: BASE64_STANDARD.encode(bytes),
                        payload_digest,
                    },
                    schema_version: SchemaVersion::WinwincodeV1,
                    sent_at: upload.created_at.clone(),
                    sequence: ExecutionSequence(sequence_i64),
                    session_identity: upload.session_identity.clone(),
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
            chunk_messages,
            ack_sequence: 0,
            final_ack: None,
            cancel_requested: false,
            replacement_authority: upload.replacement_authority.clone(),
        };
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> Result<(), AdapterStoreError> {
        let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&self.bytes)));
        let final_sequence = self.final_sequence()?;
        let exact_authority_key = authority_key(
            &self.open_message.lease,
            &self.open_message.worker_session_id,
            &self.open_message.session_identity,
        )?;
        let exact_artifact_id = canonical_id(
            "art",
            b"winwincode.candidate-artifact.v1",
            &[
                exact_authority_key.as_bytes(),
                self.descriptor.digest.0.as_bytes(),
            ],
        );
        if !candidate_artifact_role(&self.execution_profile)
            || self.bytes.is_empty()
            || !lowercase_sha256(&self.job_digest.0)
            || !lowercase_sha256(&self.logical_job_digest.0)
            || !scope_matches_session(&self.scope, &self.open_message.session_identity)
            || self.authority_key != exact_authority_key
            || digest != self.descriptor.digest
            || self.descriptor.artifact_id.0 != exact_artifact_id
            || self.descriptor.kind != ArtifactKind::Candidate
            || self.descriptor.media_type != CANDIDATE_MEDIA_TYPE
            || self.descriptor.file_name.as_deref() != Some(CANDIDATE_FILE_NAME)
            || usize::try_from(self.descriptor.size_bytes).ok() != Some(self.bytes.len())
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
            || self.chunk_messages.is_empty()
            || self.ack_sequence > final_sequence
            || (self.cancel_requested && self.final_ack.is_some())
        {
            return Err(AdapterStoreError::Corrupt);
        }
        if self.rebuild_chunk_bytes()? != self.bytes {
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
                || replacement.scope != self.scope
                || !lowercase_sha256(&replacement.receipt_digest.0)
                || !lowercase_sha256(&replacement.logical_job_digest.0)
            {
                return Err(AdapterStoreError::Corrupt);
            }
            if let Some(predecessor_session) = replacement.predecessor_session_identity.as_ref()
                && (predecessor_session.worker_session_id == self.open_message.worker_session_id
                    || predecessor_session.stage_run_id
                        != self.open_message.session_identity.stage_run_id
                    || predecessor_session.product_session_id
                        != self.open_message.session_identity.product_session_id)
            {
                return Err(AdapterStoreError::Corrupt);
            }
        }
        if let Some(ack) = &self.final_ack {
            self.validate_ack_authority(ack)?;
            if !matches!(
                ack.status,
                LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
            ) || u64::try_from(ack.ack_sequence.0).ok() != Some(final_sequence)
                || ack.replay_from_sequence.is_some()
                || ack.error.is_some()
                || self.ack_sequence != final_sequence
            {
                return Err(AdapterStoreError::Corrupt);
            }
        }
        Ok(())
    }

    fn rebuild_chunk_bytes(&self) -> Result<Vec<u8>, AdapterStoreError> {
        if self.chunk_messages.len() != self.bytes.chunks(RAW_CHUNK_BYTES).len() {
            return Err(AdapterStoreError::Corrupt);
        }
        self.chunk_messages
            .iter()
            .zip(self.bytes.chunks(RAW_CHUNK_BYTES))
            .enumerate()
            .try_fold(Vec::new(), |mut bytes, (index, (chunk, expected_bytes))| {
                let expected = i64::try_from(index + 1).map_err(|_| AdapterStoreError::Corrupt)?;
                let decoded = BASE64_STANDARD
                    .decode(&chunk.payload.data_base64)
                    .map_err(|_| AdapterStoreError::Corrupt)?;
                let sequence = u64::try_from(expected).map_err(|_| AdapterStoreError::Corrupt)?;
                if decoded.as_slice() != expected_bytes
                    || chunk.artifact_id != self.descriptor.artifact_id
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
                    || chunk.payload.content_type != CANDIDATE_MEDIA_TYPE
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
            && self.job_digest == other.job_digest
            && self.logical_job_digest == other.logical_job_digest
            && self.execution_profile == other.execution_profile
            && self.scope == other.scope
            && self.bytes == other.bytes
            && self.descriptor == other.descriptor
            && self.replacement_authority == other.replacement_authority
    }

    fn final_sequence(&self) -> Result<u64, AdapterStoreError> {
        u64::try_from(self.chunk_messages.len()).map_err(|_| AdapterStoreError::Corrupt)
    }

    fn reference(&self) -> ArtifactReference {
        ArtifactReference {
            artifact_id: self.descriptor.artifact_id.clone(),
            digest: self.descriptor.digest.clone(),
        }
    }

    fn authority(&self) -> CandidateArtifactAuthority {
        CandidateArtifactAuthority {
            job_digest: self.job_digest.clone(),
            logical_job_digest: self.logical_job_digest.clone(),
            execution_profile: self.execution_profile.clone(),
            scope: self.scope.clone(),
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
        || record.scope != upload.scope
        || record.bytes != upload.bytes
        || record.descriptor.digest != upload.digest
    {
        return Err(AdapterStoreError::Conflict);
    }
    Ok(Some(record))
}

fn validate_upload(upload: &CandidateArtifactUpload) -> Result<(), AdapterStoreError> {
    let actual_digest = format!("sha256:{:x}", Sha256::digest(&upload.bytes));
    if !candidate_artifact_role(&upload.execution_profile)
        || upload.bytes.is_empty()
        || upload.digest.0 != actual_digest
        || !lowercase_sha256(&upload.job_digest.0)
        || !lowercase_sha256(&upload.logical_job_digest.0)
        || !scope_matches_session(&upload.scope, &upload.session_identity)
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
        || replacement.scope != upload.scope
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
        "executor" | "remediator" | "reviewer" | "verifier" | "adversarial-verifier"
    )
}

fn scope_matches_session(
    scope: &ExecutionScope,
    session: &winwincode_domain::SessionIdentity,
) -> bool {
    match scope {
        ExecutionScope::ProductSessionExecutionScope(scope) => {
            session.product_session_id == scope.product_session_id && session.stage_run_id.is_none()
        }
        ExecutionScope::DeliveryStageExecutionScope(scope) => {
            session.product_session_id == scope.product_session_id
                && session.stage_run_id.as_ref() == Some(&scope.stage_run_id)
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
    let bytes = serde_json::to_vec(record).map_err(|_| AdapterStoreError::Corrupt)?;
    transaction
        .execute(
            "INSERT INTO candidate_artifact_upload(authority_key, artifact_id, record_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(authority_key) DO UPDATE SET record_json = excluded.record_json",
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

    let mut statement = transaction
        .prepare("SELECT record_json FROM candidate_artifact_upload ORDER BY authority_key")
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let rows = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let mut match_record = None;
    for row in rows {
        let bytes = row.map_err(|_| AdapterStoreError::Unavailable)?;
        let Some(record) = decode_record(Some(bytes))? else {
            return Err(AdapterStoreError::Corrupt);
        };
        record.validate()?;
        let predecessor_lease = &record.open_message.lease;
        if predecessor_lease.job_id == authority.lease.job_id
            && record.logical_job_digest == authority.logical_job_digest
            && record.execution_profile == authority.execution_profile
            && record.scope == authority.scope
            && predecessor_lease.attempt.saturating_add(1) == authority.lease.attempt
            && predecessor_lease.worker_id == authority.lease.worker_id
            && predecessor_lease.worker_instance_id != authority.lease.worker_instance_id
            && match_record.replace(record).is_some()
        {
            return Err(AdapterStoreError::Conflict);
        }
    }
    Ok(match_record.map(|record| (record, false)))
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
        .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt))
        .transpose()
}

fn final_ack_matches_retry(retained: &ArtifactAckMessage, retry: &ArtifactAckMessage) -> bool {
    retained.ack_sequence == retry.ack_sequence
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
    for chunk in &record.chunk_messages {
        let sequence = u64::try_from(chunk.sequence.0).map_err(|_| AdapterStoreError::Corrupt)?;
        if sequence <= acknowledged {
            delete_delivery(transaction, &chunk.message_id.0)?;
        }
    }
    Ok(())
}

fn requeue_suffix(
    transaction: &Transaction<'_>,
    record: &StoredCandidateArtifact,
    replay_from: u64,
) -> Result<Vec<DurableExecutionDelivery>, AdapterStoreError> {
    let mut replay = Vec::new();
    for chunk in &record.chunk_messages {
        let sequence = u64::try_from(chunk.sequence.0).map_err(|_| AdapterStoreError::Corrupt)?;
        if sequence >= replay_from {
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
        ArtifactId, CodexThreadId, DeliveryId, ExecutionAckSequence, ExecutionMessageId,
        FencingToken, LeaseId, ProductSessionId, RequestId, Sha256Digest, StageRunId,
        WorkerInstanceId, WorkerSessionId,
    };
    use winwincode_execution_port::generated::{
        ArtifactAckMessageKind, DeliveryStageExecutionScope, DeliveryStageExecutionScopeKind,
        ExecutionPortError, ExecutionPortErrorCode,
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
        let value = fixture["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find(|message| message["kind"] == "artifact.open")
            .expect("artifact.open")
            .clone();
        serde_json::from_value(value).expect("generated artifact.open")
    }

    fn upload(bytes: Vec<u8>) -> CandidateArtifactUpload {
        let open = artifact_open_fixture();
        let stage_run_id = open
            .session_identity
            .stage_run_id
            .clone()
            .expect("Delivery stage fixture");
        CandidateArtifactUpload {
            job_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(b"exact job"))),
            logical_job_digest: Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(b"logical job")
            )),
            execution_profile: "executor".into(),
            scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
                delivery_id: DeliveryId("dlv_00000000000000000000000001".to_owned()),
                delivery_task_id: None,
                kind: DeliveryStageExecutionScopeKind::DeliveryStage,
                product_session_id: ProductSessionId(
                    open.session_identity.product_session_id.0.clone(),
                ),
                rework_authorization: None,
                stage_run_id: StageRunId(stage_run_id.0.clone()),
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
        successor.session_identity.codex_thread_id =
            CodexThreadId("cdx_0000000000000000000000000H".to_owned());
        let stage_run_id = predecessor
            .session_identity
            .stage_run_id
            .clone()
            .expect("Delivery stage fixture");
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
            scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
                delivery_id: DeliveryId("dlv_00000000000000000000000001".to_owned()),
                delivery_task_id: None,
                kind: DeliveryStageExecutionScopeKind::DeliveryStage,
                product_session_id: predecessor.session_identity.product_session_id.clone(),
                rework_authorization: None,
                stage_run_id,
            }),
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
        let root = test_root("retain-restart");
        let upload = upload(br#"{"candidate":"exact"}"#.to_vec());
        let original;
        let retained;
        {
            let (candidate, execution) = open_ledgers(&root).expect("open ledgers");
            retained = candidate.retain(&upload).expect("retain candidate");
            assert_eq!(retained.deliveries.len(), 2);
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
                Err(AdapterStoreError::Conflict)
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
            let replay = candidate
                .retain(&upload)
                .expect("new attempt after cancellation");
            assert_eq!(replay.artifact, retained.artifact);
            assert_eq!(replay.deliveries.len(), 2);
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
        std::fs::remove_dir_all(root).expect("remove fixture");
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

        let retained = candidate.retain(&upload).expect("retain after cancel");
        assert_eq!(retained.artifact, first.artifact);
        let final_ack = acknowledgement(&retained, &upload, 1, LeaseWriteStatus::Accepted);
        assert_eq!(
            candidate.apply_ack(&final_ack).expect("accept candidate"),
            CandidateArtifactAckOutcome::Accepted(retained.artifact.clone())
        );
        assert_eq!(
            candidate.cancel(&upload.authority()),
            Err(AdapterStoreError::Conflict)
        );
        assert_eq!(
            candidate
                .accepted_reference(&upload.authority())
                .expect("accepted reference survives cancel"),
            Some(retained.artifact)
        );
        std::fs::remove_dir_all(root).expect("remove fixture");
    }
}
