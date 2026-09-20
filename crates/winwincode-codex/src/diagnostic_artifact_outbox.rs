// SPDX-License-Identifier: Apache-2.0

//! Durable command/test output Artifact uploads.
//!
//! This is deliberately a small companion to the candidate upload ledger. It
//! retains the exact bounded, redacted bytes and the generated
//! `artifact.open`/`artifact.chunk` frames before transport. Runtime evidence
//! may only publish the reference returned after the final Artifact ACK.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::{OptionalExtension as _, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{
    ArtifactId, ExecutionMessageId, ExecutionSequence, Instant, RequestId, SchemaVersion,
    SessionIdentity, Sha256Digest, WorkerSessionId,
};
use winwincode_execution_port::generated::{
    ArtifactAckMessage, ArtifactChunkMessage, ArtifactChunkMessageKind, ArtifactDescriptor,
    ArtifactKind, ArtifactOpenMessage, ArtifactOpenMessageKind, ArtifactReference, EncodedPayload,
    ExecutionJob, ExecutionLeaseStamp, ExecutionPortMessage, ExecutionScope, LeaseWriteStatus,
};

use crate::{
    DurableExecutionDelivery,
    outbox::ExecutionOutbox,
    store::{AdapterStore, AdapterStoreError},
};

/// A bounded, already-redacted command/test output stream.
#[derive(Clone, Debug, PartialEq)]
pub struct DiagnosticArtifactUpload {
    pub run_key: String,
    pub job: ExecutionJob,
    pub scope: ExecutionScope,
    pub lease: ExecutionLeaseStamp,
    pub worker_session_id: WorkerSessionId,
    pub session_identity: SessionIdentity,
    pub source_id: String,
    pub kind: ArtifactKind,
    pub media_type: String,
    pub file_name: String,
    pub bytes: Vec<u8>,
    pub created_at: Instant,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiagnosticArtifactAuthority {
    pub job: ExecutionJob,
    pub scope: ExecutionScope,
    pub lease: ExecutionLeaseStamp,
    pub worker_session_id: WorkerSessionId,
    pub session_identity: SessionIdentity,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RetainedDiagnosticArtifact {
    pub run_key: String,
    pub artifact: ArtifactReference,
    pub authority: DiagnosticArtifactAuthority,
    pub deliveries: Vec<DurableExecutionDelivery>,
    pub already_accepted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DiagnosticArtifactAckOutcome {
    Unknown,
    Pending,
    Replay(Vec<DurableExecutionDelivery>),
    Accepted {
        reference: ArtifactReference,
        authority: DiagnosticArtifactAuthority,
        run_key: String,
    },
    Duplicate {
        reference: ArtifactReference,
        authority: DiagnosticArtifactAuthority,
        run_key: String,
    },
}

const DIAGNOSTIC_CHUNK_BYTES: usize = 64 * 1024;
const PENDING: &str = "pending";

/// Maximum bytes retained for one command/test output Artifact.
pub const MAX_DIAGNOSTIC_OUTPUT_BYTES: usize = 256 * 1024;
/// Keep individual model/body lines bounded even when the whole output is small.
pub const MAX_DIAGNOSTIC_LINE_BYTES: usize = 1_024;

/// Bounds and redacts Core command output before it becomes durable.
///
/// This keeps the output useful for diagnosis while applying the same safety
/// boundary as the Device log recorder: control bytes are removed, common
/// credential-bearing lines are replaced, and local absolute paths are
/// masked. The raw Core `formatted_output` is intentionally excluded because
/// it is model-facing content rather than process output.
pub fn sanitize_command_output(stdout: &str, stderr: &str) -> Vec<u8> {
    let mut result = Vec::new();
    append_sanitized(&mut result, "[stdout]\n", stdout);
    append_sanitized(&mut result, "[stderr]\n", stderr);
    result
}

/// Stable opaque source identity for a Core command invocation.
///
/// Call IDs are transport correlation data and must not become externally
/// visible artifact names or source identifiers.
pub fn canonical_command_source_id(turn_id: &str, call_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.diagnostic-command-source.v1");
    digest.update((turn_id.len() as u64).to_be_bytes());
    digest.update(turn_id.as_bytes());
    digest.update((call_id.len() as u64).to_be_bytes());
    digest.update(call_id.as_bytes());
    format!("command_{}", &format!("{:x}", digest.finalize())[..32])
}

fn append_sanitized(output: &mut Vec<u8>, label: &str, source: &str) {
    if output.len().saturating_add(label.len()) > MAX_DIAGNOSTIC_OUTPUT_BYTES {
        return;
    }
    output.extend_from_slice(label.as_bytes());
    for line in source.lines() {
        if output.len() >= MAX_DIAGNOSTIC_OUTPUT_BYTES {
            break;
        }
        let cleaned: String = line
            .chars()
            .filter(|ch| !ch.is_control() || *ch == '\t')
            .collect();
        // Device worker_logs drops an overlong line as a whole. Doing the
        // same here avoids a byte truncation that could split UTF-8.
        if cleaned.len() > MAX_DIAGNOSTIC_LINE_BYTES {
            continue;
        }
        let line = if contains_credential_material(&cleaned) {
            "[REDACTED credential]".to_owned()
        } else {
            mask_absolute_paths(&cleaned)
        };
        if output.len().saturating_add(line.len()).saturating_add(1) > MAX_DIAGNOSTIC_OUTPUT_BYTES {
            break;
        }
        output.extend_from_slice(line.as_bytes());
        output.push(b'\n');
    }
}

const CREDENTIAL_MARKERS: &[&str] = &[
    "--api-key ",
    "--password ",
    "--secret ",
    "--token ",
    "api_key=",
    "api-key:",
    "apikey=",
    "authorization:",
    "authorization=",
    "aws_secret_access_key",
    "bearer ",
    "credential=",
    "gho_",
    "ghp_",
    "ghr_",
    "ghs_",
    "ghu_",
    "github_pat_",
    "glpat-",
    "-----begin private key-----",
    "-----begin rsa private key-----",
    "-----begin ecdsa private key-----",
    "-----begin openssh private key-----",
    "password=",
    "private key",
    "secret=",
    "set-cookie:",
    "sk-ant-",
    "sk-proj-",
    "sk-svcacct-",
    "token=",
    "wsc-",
    "wsc_",
    "workercredential",
    "x-api-key:",
    "x-app-",
    "xapp-",
    "xoxb-",
    "xoxp-",
];

fn contains_credential_material(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    CREDENTIAL_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
        || has_long_hex(&lower)
        || has_url_userinfo(&lower)
}

fn has_long_hex(value: &str) -> bool {
    let mut run = 0;
    for byte in value.bytes() {
        if byte.is_ascii_hexdigit() {
            run += 1;
            if run >= 32 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

fn has_url_userinfo(value: &str) -> bool {
    value.split_whitespace().any(|part| {
        part.find("://")
            .and_then(|start| {
                part[start + 3..]
                    .find('@')
                    .map(|at| &part[start + 3..start + 3 + at])
            })
            .is_some_and(|authority| authority.contains(':'))
    })
}

fn mask_absolute_paths(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut masked = String::with_capacity(line.len());
    let mut position = 0;
    let mut previous = None;
    while position < bytes.len() {
        let byte = bytes[position];
        if (byte == b'f' || byte == b'F')
            && bytes[position..].len() >= 7
            && bytes[position..position + 7].eq_ignore_ascii_case(b"file://")
        {
            let end = url_end(bytes, position + 7);
            masked.push_str("[PATH]");
            previous = Some(char::from(bytes[end.saturating_sub(1)]));
            position = end;
            continue;
        }
        if byte == b'\\'
            && position + 2 < bytes.len()
            && bytes[position + 1] == b'\\'
            && bytes[position + 2] != b'\\'
            && is_path_boundary(previous)
        {
            let end = consume_windows_run(bytes, position);
            if end > position + 2 {
                masked.push_str("[PATH]");
                previous = Some(char::from(bytes[end - 1]));
                position = end;
                continue;
            }
        }
        if byte.is_ascii_alphabetic()
            && position + 2 < bytes.len()
            && bytes[position + 1] == b':'
            && (bytes[position + 2] == b'/' || bytes[position + 2] == b'\\')
            && is_path_boundary(previous)
        {
            let end = consume_windows_run(bytes, position + 2);
            if end >= position + 4 {
                masked.push_str("[PATH]");
                previous = Some(char::from(bytes[end - 1]));
                position = end;
                continue;
            }
        }
        if byte == b'~'
            && position + 1 < bytes.len()
            && bytes[position + 1] == b'/'
            && is_path_boundary(previous)
        {
            let end = consume_posix_run(bytes, position + 1);
            if end > position + 2 {
                masked.push_str("[PATH]");
                previous = Some(char::from(bytes[end - 1]));
                position = end;
                continue;
            }
        }
        if byte == b'/'
            && position + 1 < bytes.len()
            && bytes[position + 1] != b'/'
            && is_path_boundary(previous)
        {
            let end = consume_posix_run(bytes, position);
            let run = &line[position..end];
            if run.len() >= 2 && run[1..].chars().any(|ch| ch != '/') {
                masked.push_str("[PATH]");
                previous = Some(char::from(bytes[end - 1]));
                position = end;
                continue;
            }
        }
        let character = line[position..]
            .chars()
            .next()
            .unwrap_or(char::REPLACEMENT_CHARACTER);
        masked.push(character);
        previous = Some(character);
        position += character.len_utf8();
    }
    masked
}

fn is_path_boundary(previous: Option<char>) -> bool {
    previous
        .is_none_or(|ch| !ch.is_ascii_alphanumeric() && !matches!(ch, '/' | '\\' | '.' | '_' | '-'))
}

fn url_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len()
        && !bytes[end].is_ascii_whitespace()
        && !matches!(bytes[end], b'"' | b'\'' | b'<' | b'>' | b')' | b']')
    {
        end += 1;
    }
    end
}

fn consume_posix_run(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len()
        && (bytes[end].is_ascii_alphanumeric()
            || matches!(bytes[end], b'/' | b'.' | b'_' | b'-')
            || bytes[end] >= 0x80)
    {
        end += 1;
    }
    end
}

fn consume_windows_run(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len()
        && (bytes[end].is_ascii_alphanumeric()
            || matches!(bytes[end], b'\\' | b'/' | b'.' | b'_' | b'-')
            || bytes[end] >= 0x80)
    {
        end += 1;
    }
    end
}

#[derive(Clone, Debug)]
pub(crate) struct DiagnosticArtifactOutbox {
    store: AdapterStore,
}

impl DiagnosticArtifactOutbox {
    pub(crate) fn open(store: AdapterStore) -> Result<Self, AdapterStoreError> {
        store
            .lock()?
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS diagnostic_artifact_upload (
               artifact_id TEXT PRIMARY KEY NOT NULL,
               authority_key TEXT NOT NULL,
               record_json BLOB NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS diagnostic_artifact_authority_idx
               ON diagnostic_artifact_upload(authority_key);",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(Self { store })
    }

    pub(crate) fn retain(
        &self,
        upload: &DiagnosticArtifactUpload,
    ) -> Result<RetainedDiagnosticArtifact, AdapterStoreError> {
        let record = StoredDiagnosticArtifact::from_upload(upload)?;
        self.store.transaction(|tx| {
            if let Some(existing) = load_by_authority(tx, &record.authority_key)? {
                existing.validate()?;
                if existing != record {
                    return Err(AdapterStoreError::Conflict);
                }
                return Ok(RetainedDiagnosticArtifact {
                    artifact: existing.reference(),
                    authority: existing.authority(),
                    run_key: existing.run_key.clone(),
                    deliveries: Vec::new(),
                    already_accepted: existing.final_ack.is_some(),
                });
            }
            let mut deliveries = Vec::with_capacity(record.chunks.len() + 1);
            deliveries.push(ExecutionOutbox::retain_in_transaction(
                tx,
                &ExecutionPortMessage::ArtifactOpenMessage(record.open.clone()),
            )?);
            for chunk in &record.chunks {
                deliveries.push(ExecutionOutbox::retain_in_transaction(
                    tx,
                    &ExecutionPortMessage::ArtifactChunkMessage(chunk.clone()),
                )?);
            }
            save(tx, &record)?;
            Ok(RetainedDiagnosticArtifact {
                artifact: record.reference(),
                authority: record.authority(),
                run_key: record.run_key.clone(),
                deliveries,
                already_accepted: false,
            })
        })
    }

    pub(crate) fn apply_ack(
        &self,
        ack: &ArtifactAckMessage,
    ) -> Result<DiagnosticArtifactAckOutcome, AdapterStoreError> {
        self.store.transaction(|tx| {
            let Some(mut record) = load_by_artifact(tx, &ack.artifact_id)? else {
                return Ok(DiagnosticArtifactAckOutcome::Unknown);
            };
            record.validate()?;
            record.validate_ack(ack)?;
            if let Some(final_ack) = &record.final_ack {
                return if final_ack_matches(final_ack, ack) {
                    Ok(DiagnosticArtifactAckOutcome::Duplicate {
                        reference: record.reference(),
                        authority: record.authority(),
                        run_key: record.run_key.clone(),
                    })
                } else {
                    Err(AdapterStoreError::Conflict)
                };
            }
            let acknowledged =
                u64::try_from(ack.ack_sequence.0).map_err(|_| AdapterStoreError::Conflict)?;
            let final_sequence =
                u64::try_from(record.chunks.len()).map_err(|_| AdapterStoreError::Corrupt)?;
            if acknowledged < record.ack_sequence || acknowledged > final_sequence {
                return Err(AdapterStoreError::Conflict);
            }
            match ack.status {
                LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate => {
                    if ack.replay_from_sequence.is_some() || ack.error.is_some() {
                        return Err(AdapterStoreError::Conflict);
                    }
                    compact(tx, &record, acknowledged)?;
                    record.ack_sequence = acknowledged;
                    if acknowledged == final_sequence {
                        record.final_ack = Some(ack.clone());
                    }
                    save(tx, &record)?;
                    Ok(if record.final_ack.is_some() {
                        DiagnosticArtifactAckOutcome::Accepted {
                            reference: record.reference(),
                            authority: record.authority(),
                            run_key: record.run_key.clone(),
                        }
                    } else {
                        DiagnosticArtifactAckOutcome::Pending
                    })
                }
                LeaseWriteStatus::Gap => {
                    let from = ack
                        .replay_from_sequence
                        .as_ref()
                        .and_then(|s| u64::try_from(s.0).ok())
                        .ok_or(AdapterStoreError::Conflict)?;
                    if acknowledged >= final_sequence
                        || from != acknowledged.saturating_add(1)
                        || ack.error.is_none()
                    {
                        return Err(AdapterStoreError::Conflict);
                    }
                    compact(tx, &record, acknowledged)?;
                    let replay = requeue(tx, &record, from)?;
                    record.ack_sequence = acknowledged;
                    save(tx, &record)?;
                    Ok(DiagnosticArtifactAckOutcome::Replay(replay))
                }
                _ => Err(AdapterStoreError::Conflict),
            }
        })
    }

    pub(crate) fn accepted_references(
        &self,
        authority: &DiagnosticArtifactAuthority,
    ) -> Result<Vec<ArtifactReference>, AdapterStoreError> {
        let conn = self.store.lock()?;
        let mut stmt = conn
            .prepare("SELECT record_json FROM diagnostic_artifact_upload ORDER BY artifact_id ASC")
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let records = stmt
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let mut result = Vec::new();
        for row in records {
            let bytes = row.map_err(|_| AdapterStoreError::Unavailable)?;
            let record: StoredDiagnosticArtifact =
                serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
            record.validate()?;
            if record.authority() == *authority && record.final_ack.is_some() {
                result.push(record.reference());
            }
        }
        Ok(result)
    }

    pub(crate) fn has_pending(
        &self,
        authority: &DiagnosticArtifactAuthority,
    ) -> Result<bool, AdapterStoreError> {
        let conn = self.store.lock()?;
        let mut stmt = conn
            .prepare("SELECT record_json FROM diagnostic_artifact_upload")
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let records = stmt
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|_| AdapterStoreError::Unavailable)?;
        for row in records {
            let record: StoredDiagnosticArtifact =
                serde_json::from_slice(&row.map_err(|_| AdapterStoreError::Unavailable)?)
                    .map_err(|_| AdapterStoreError::Corrupt)?;
            record.validate()?;
            if record.authority() == *authority && record.final_ack.is_none() {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct StoredDiagnosticArtifact {
    authority_key: String,
    run_key: String,
    job: ExecutionJob,
    scope: ExecutionScope,
    source_id: String,
    bytes: Vec<u8>,
    descriptor: ArtifactDescriptor,
    open: ArtifactOpenMessage,
    chunks: Vec<ArtifactChunkMessage>,
    ack_sequence: u64,
    final_ack: Option<ArtifactAckMessage>,
}

impl StoredDiagnosticArtifact {
    #[allow(clippy::too_many_lines)]
    fn from_upload(upload: &DiagnosticArtifactUpload) -> Result<Self, AdapterStoreError> {
        if upload.run_key.is_empty()
            || upload.bytes.is_empty()
            || upload.bytes.len() > MAX_DIAGNOSTIC_OUTPUT_BYTES
            || upload.source_id.is_empty()
            || upload.source_id.len() > 512
            || !matches!(
                upload.kind,
                ArtifactKind::CommandOutput | ArtifactKind::TestOutput
            )
            || upload.worker_session_id != upload.session_identity.worker_session_id
            || !scope_matches(&upload.scope, &upload.session_identity)
        {
            return Err(AdapterStoreError::Conflict);
        }
        let authority_key = authority_key(
            &upload.lease,
            &upload.worker_session_id,
            &upload.session_identity,
            &upload.source_id,
            &upload.run_key,
        )?;
        let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&upload.bytes)));
        let artifact_id = ArtifactId(canonical_id(
            "art",
            b"winwincode.diagnostic-artifact.v1",
            &[authority_key.as_bytes(), digest.0.as_bytes()],
        ));
        let descriptor = ArtifactDescriptor {
            artifact_id: artifact_id.clone(),
            digest,
            file_name: Some(upload.file_name.clone()),
            kind: upload.kind.clone(),
            media_type: upload.media_type.clone(),
            size_bytes: i64::try_from(upload.bytes.len())
                .map_err(|_| AdapterStoreError::Conflict)?,
        };
        let open = ArtifactOpenMessage {
            artifact: descriptor.clone(),
            kind: ArtifactOpenMessageKind::ArtifactOpen,
            lease: upload.lease.clone(),
            message_id: ExecutionMessageId(canonical_id(
                "xmsg",
                b"winwincode.diagnostic-artifact.open.v1",
                &[artifact_id.0.as_bytes()],
            )),
            request_id: RequestId(canonical_id(
                "req",
                b"winwincode.diagnostic-artifact.request.v1",
                &[artifact_id.0.as_bytes()],
            )),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: upload.created_at.clone(),
            session_identity: upload.session_identity.clone(),
            worker_session_id: upload.worker_session_id.clone(),
        };
        let chunks = upload
            .bytes
            .chunks(DIAGNOSTIC_CHUNK_BYTES)
            .enumerate()
            .map(|(index, bytes)| {
                let sequence = u64::try_from(index + 1).map_err(|_| AdapterStoreError::Conflict)?;
                let data = STANDARD.encode(bytes);
                Ok(ArtifactChunkMessage {
                    artifact_id: artifact_id.clone(),
                    is_final: index + 1 == upload.bytes.chunks(DIAGNOSTIC_CHUNK_BYTES).len(),
                    kind: ArtifactChunkMessageKind::ArtifactChunk,
                    lease: upload.lease.clone(),
                    message_id: ExecutionMessageId(canonical_id(
                        "xmsg",
                        b"winwincode.diagnostic-artifact.chunk.v1",
                        &[artifact_id.0.as_bytes(), &sequence.to_be_bytes()],
                    )),
                    payload: EncodedPayload {
                        content_type: upload.media_type.clone(),
                        data_base64: data,
                        payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
                    },
                    schema_version: SchemaVersion::WinwincodeV1,
                    sent_at: upload.created_at.clone(),
                    sequence: ExecutionSequence(
                        i64::try_from(sequence).map_err(|_| AdapterStoreError::Conflict)?,
                    ),
                    session_identity: upload.session_identity.clone(),
                    worker_session_id: upload.worker_session_id.clone(),
                })
            })
            .collect::<Result<Vec<_>, AdapterStoreError>>()?;
        let record = Self {
            authority_key,
            run_key: upload.run_key.clone(),
            job: upload.job.clone(),
            scope: upload.scope.clone(),
            source_id: upload.source_id.clone(),
            bytes: upload.bytes.clone(),
            descriptor,
            open,
            chunks,
            ack_sequence: 0,
            final_ack: None,
        };
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> Result<(), AdapterStoreError> {
        let exact_authority_key = authority_key(
            &self.open.lease,
            &self.open.worker_session_id,
            &self.open.session_identity,
            &self.source_id,
            &self.run_key,
        )?;
        let exact_artifact_id = canonical_id(
            "art",
            b"winwincode.diagnostic-artifact.v1",
            &[
                exact_authority_key.as_bytes(),
                self.descriptor.digest.0.as_bytes(),
            ],
        );
        let expected_file_name = format!("{}.log", self.source_id);
        let final_sequence =
            u64::try_from(self.chunks.len()).map_err(|_| AdapterStoreError::Corrupt)?;
        if self.run_key.is_empty()
            || self.bytes.is_empty()
            || self.bytes.len() > MAX_DIAGNOSTIC_OUTPUT_BYTES
            || self.source_id.is_empty()
            || !scope_matches(&self.scope, &self.open.session_identity)
            || self.job.job_id != self.open.lease.job_id
            || self.job.scope != self.scope
            || self.authority_key != exact_authority_key
            || self.descriptor.size_bytes != i64::try_from(self.bytes.len()).unwrap_or(-1)
            || self.descriptor.digest.0 != format!("sha256:{:x}", Sha256::digest(&self.bytes))
            || self.descriptor.artifact_id.0 != exact_artifact_id
            || self.descriptor.file_name.as_deref() != Some(expected_file_name.as_str())
            || self.descriptor.media_type != "text/plain; charset=utf-8"
            || !matches!(
                self.descriptor.kind,
                ArtifactKind::CommandOutput | ArtifactKind::TestOutput
            )
            || self.open.artifact != self.descriptor
            || self.open.kind != ArtifactOpenMessageKind::ArtifactOpen
            || self.open.schema_version != SchemaVersion::WinwincodeV1
            || self.open.worker_session_id != self.open.session_identity.worker_session_id
            || self.open.message_id.0
                != canonical_id(
                    "xmsg",
                    b"winwincode.diagnostic-artifact.open.v1",
                    &[self.descriptor.artifact_id.0.as_bytes()],
                )
            || self.open.request_id.0
                != canonical_id(
                    "req",
                    b"winwincode.diagnostic-artifact.request.v1",
                    &[self.descriptor.artifact_id.0.as_bytes()],
                )
            || self.chunks.is_empty()
            || self.ack_sequence > final_sequence
        {
            return Err(AdapterStoreError::Corrupt);
        }
        if self.rebuild_chunk_bytes()? != self.bytes {
            return Err(AdapterStoreError::Corrupt);
        }
        if let Some(ack) = &self.final_ack {
            self.validate_ack(ack)?;
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
        if self.chunks.len() != self.bytes.chunks(DIAGNOSTIC_CHUNK_BYTES).len() {
            return Err(AdapterStoreError::Corrupt);
        }
        self.chunks
            .iter()
            .zip(self.bytes.chunks(DIAGNOSTIC_CHUNK_BYTES))
            .enumerate()
            .try_fold(Vec::new(), |mut bytes, (index, (chunk, expected_bytes))| {
                let expected = i64::try_from(index + 1).map_err(|_| AdapterStoreError::Corrupt)?;
                let decoded = STANDARD
                    .decode(&chunk.payload.data_base64)
                    .map_err(|_| AdapterStoreError::Corrupt)?;
                let sequence = u64::try_from(index + 1).map_err(|_| AdapterStoreError::Corrupt)?;
                if decoded.as_slice() != expected_bytes
                    || chunk.artifact_id != self.descriptor.artifact_id
                    || chunk.lease != self.open.lease
                    || chunk.worker_session_id != self.open.worker_session_id
                    || chunk.session_identity != self.open.session_identity
                    || chunk.schema_version != SchemaVersion::WinwincodeV1
                    || chunk.kind != ArtifactChunkMessageKind::ArtifactChunk
                    || chunk.sent_at != self.open.sent_at
                    || chunk.sequence.0 != expected
                    || chunk.message_id.0
                        != canonical_id(
                            "xmsg",
                            b"winwincode.diagnostic-artifact.chunk.v1",
                            &[
                                self.descriptor.artifact_id.0.as_bytes(),
                                &sequence.to_be_bytes(),
                            ],
                        )
                    || chunk.is_final != (index + 1 == self.chunks.len())
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

    fn validate_ack(&self, ack: &ArtifactAckMessage) -> Result<(), AdapterStoreError> {
        if ack.artifact_id != self.descriptor.artifact_id
            || ack.lease != self.open.lease
            || ack.worker_session_id != self.open.worker_session_id
            || ack.session_identity != self.open.session_identity
            || ack.schema_version != SchemaVersion::WinwincodeV1
        {
            return Err(AdapterStoreError::Conflict);
        }
        Ok(())
    }
    fn reference(&self) -> ArtifactReference {
        ArtifactReference {
            artifact_id: self.descriptor.artifact_id.clone(),
            digest: self.descriptor.digest.clone(),
        }
    }
    fn authority(&self) -> DiagnosticArtifactAuthority {
        DiagnosticArtifactAuthority {
            job: self.job.clone(),
            scope: self.scope.clone(),
            lease: self.open.lease.clone(),
            worker_session_id: self.open.worker_session_id.clone(),
            session_identity: self.open.session_identity.clone(),
        }
    }
}

fn scope_matches(scope: &ExecutionScope, identity: &SessionIdentity) -> bool {
    match scope {
        ExecutionScope::ProductSessionExecutionScope(scope) => {
            identity.product_session_id == scope.product_session_id
                && identity.work_run_id.is_none()
        }
        ExecutionScope::WorkRunExecutionScope(scope) => {
            identity.product_session_id == scope.product_session_id
                && identity.work_run_id.as_ref() == Some(&scope.work_run_id)
        }
    }
}
fn authority_key(
    lease: &ExecutionLeaseStamp,
    worker: &WorkerSessionId,
    identity: &SessionIdentity,
    source: &str,
    run_key: &str,
) -> Result<String, AdapterStoreError> {
    let bytes = serde_json::to_vec(&(lease, worker, identity, source, run_key))
        .map_err(|_| AdapterStoreError::Corrupt)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}
fn canonical_id(prefix: &str, namespace: &[u8], parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace);
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    format!(
        "{prefix}_{}",
        &format!("{:x}", digest.finalize())[..26].to_ascii_uppercase()
    )
}
fn load_by_authority(
    tx: &Transaction<'_>,
    key: &str,
) -> Result<Option<StoredDiagnosticArtifact>, AdapterStoreError> {
    let bytes = tx
        .query_row(
            "SELECT record_json FROM diagnostic_artifact_upload WHERE authority_key = ?1",
            params![key],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    bytes
        .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt))
        .transpose()
}
fn load_by_artifact(
    tx: &Transaction<'_>,
    id: &ArtifactId,
) -> Result<Option<StoredDiagnosticArtifact>, AdapterStoreError> {
    let bytes = tx
        .query_row(
            "SELECT record_json FROM diagnostic_artifact_upload WHERE artifact_id = ?1",
            params![id.0],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    bytes
        .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt))
        .transpose()
}
fn save(tx: &Transaction<'_>, record: &StoredDiagnosticArtifact) -> Result<(), AdapterStoreError> {
    let bytes = serde_json::to_vec(record).map_err(|_| AdapterStoreError::Corrupt)?;
    tx.execute("INSERT INTO diagnostic_artifact_upload(artifact_id, authority_key, record_json) VALUES (?1, ?2, ?3) ON CONFLICT(artifact_id) DO UPDATE SET record_json=excluded.record_json", params![record.descriptor.artifact_id.0, record.authority_key, bytes]).map_err(|_| AdapterStoreError::Unavailable)?;
    Ok(())
}
fn compact(
    tx: &Transaction<'_>,
    record: &StoredDiagnosticArtifact,
    ack: u64,
) -> Result<(), AdapterStoreError> {
    tx.execute(
        "DELETE FROM execution_outbox WHERE delivery_id = ?1",
        params![record.open.message_id.0],
    )
    .map_err(|_| AdapterStoreError::Unavailable)?;
    for chunk in &record.chunks {
        if u64::try_from(chunk.sequence.0)
            .ok()
            .is_some_and(|n| n <= ack)
        {
            tx.execute(
                "DELETE FROM execution_outbox WHERE delivery_id = ?1",
                params![chunk.message_id.0],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        }
    }
    Ok(())
}
fn requeue(
    tx: &Transaction<'_>,
    record: &StoredDiagnosticArtifact,
    from: u64,
) -> Result<Vec<DurableExecutionDelivery>, AdapterStoreError> {
    let mut replay = Vec::new();
    for chunk in &record.chunks {
        if u64::try_from(chunk.sequence.0)
            .ok()
            .is_some_and(|n| n >= from)
        {
            let changed = tx
                .execute(
                    "UPDATE execution_outbox SET state = ?1 WHERE delivery_id = ?2",
                    params![PENDING, chunk.message_id.0],
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
fn final_ack_matches(left: &ArtifactAckMessage, right: &ArtifactAckMessage) -> bool {
    left.artifact_id == right.artifact_id
        && left.ack_sequence == right.ack_sequence
        && left.lease == right.lease
        && left.worker_session_id == right.worker_session_id
        && left.session_identity == right.session_identity
        && is_final_ack_status(&left.status)
        && is_final_ack_status(&right.status)
        && left.kind == right.kind
        && left.message_id == right.message_id
        && left.schema_version == right.schema_version
        && left.sent_at == right.sent_at
        && left.error == right.error
        && left.replay_from_sequence == right.replay_from_sequence
}

fn is_final_ack_status(status: &LeaseWriteStatus) -> bool {
    matches!(
        status,
        LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use winwincode_domain::{ExecutionAckSequence, ExecutionSequence};
    use winwincode_execution_port::generated::{
        ArtifactAckMessageKind, ExecutionPortError, ExecutionPortErrorCode,
    };

    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "winwincode-diagnostic-outbox-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    fn fixture_upload(bytes: Vec<u8>) -> DiagnosticArtifactUpload {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .expect("execution fixture");
        let job_value = fixture["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find(|message| message["kind"] == "job.dispatch")
            .and_then(|message| message.get("job"))
            .expect("job dispatch");
        let job: ExecutionJob = serde_json::from_value(job_value.clone()).expect("job");
        let open_value = fixture["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find(|message| message["kind"] == "artifact.open")
            .expect("artifact open");
        let open: ArtifactOpenMessage = serde_json::from_value(open_value.clone()).expect("open");
        DiagnosticArtifactUpload {
            run_key: "run_fixture_opaque".to_owned(),
            scope: job.scope.clone(),
            job,
            lease: open.lease,
            worker_session_id: open.worker_session_id,
            session_identity: open.session_identity,
            source_id: "command_opaque_fixture".to_owned(),
            kind: ArtifactKind::CommandOutput,
            media_type: "text/plain; charset=utf-8".to_owned(),
            file_name: "command_opaque_fixture.log".to_owned(),
            bytes,
            created_at: open.sent_at,
        }
    }

    fn ack(
        upload: &DiagnosticArtifactUpload,
        retained: &RetainedDiagnosticArtifact,
    ) -> ArtifactAckMessage {
        ArtifactAckMessage {
            ack_sequence: ExecutionAckSequence(
                i64::try_from(upload.bytes.chunks(DIAGNOSTIC_CHUNK_BYTES).len())
                    .expect("diagnostic chunk count"),
            ),
            artifact_id: retained.artifact.artifact_id.clone(),
            error: None,
            kind: ArtifactAckMessageKind::ArtifactAck,
            lease: upload.lease.clone(),
            message_id: ExecutionMessageId("xmsg_00000000000000000000009999".to_owned()),
            replay_from_sequence: None,
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: upload.created_at.clone(),
            session_identity: upload.session_identity.clone(),
            status: LeaseWriteStatus::Accepted,
            worker_session_id: upload.worker_session_id.clone(),
        }
    }

    #[test]
    fn sanitizer_matches_worker_log_credential_and_path_boundaries() {
        let output = "wsc-live-material\n".to_owned()
            + "glpat-secret\n"
            + "X-API-Key: hidden\n"
            + "unc \\\\server\\share\\secret.txt\n"
            + "file:///Users/alice/private.txt\n"
            + "unicode /tmp/秘密.txt\n"
            + "https://example.com/public/path\n";
        let sanitized = String::from_utf8(sanitize_command_output(&output, "")).expect("utf8");
        assert!(!sanitized.contains("wsc-live-material"));
        assert!(!sanitized.contains("glpat-secret"));
        assert!(!sanitized.contains("hidden"));
        assert!(!sanitized.contains("/Users/alice/private.txt"));
        assert!(!sanitized.contains("\\\\server\\share\\secret.txt"));
        assert!(!sanitized.contains("file:///Users"));
        assert!(sanitized.contains("https://example.com/public/path"));
        assert!(sanitized.contains("[PATH]"));
    }

    #[test]
    fn sanitizer_drops_long_lines_without_splitting_unicode_or_output_cap() {
        let long = format!("{}終", "x".repeat(MAX_DIAGNOSTIC_LINE_BYTES));
        let sanitized = sanitize_command_output(&format!("{long}\n短い行"), "");
        let text = String::from_utf8(sanitized.clone()).expect("sanitizer preserves utf8");
        assert!(!text.contains(&long));
        assert!(text.contains("短い行"));
        assert!(sanitized.len() <= MAX_DIAGNOSTIC_OUTPUT_BYTES);
    }

    #[test]
    fn command_source_identity_is_opaque() {
        let source = canonical_command_source_id("turn-secret", "call-secret");
        assert!(!source.contains("turn-secret"));
        assert!(!source.contains("call-secret"));
        assert!(source.starts_with("command_"));
    }

    #[test]
    fn upload_ack_restart_and_foreign_ack_keep_one_authoritative_reference() {
        let root = test_root("ack-restart");
        let upload = fixture_upload(
            format!(
                "{}\n中文\nstderr\n",
                "stdout\n".repeat(DIAGNOSTIC_CHUNK_BYTES / 4)
            )
            .into_bytes(),
        );
        let retained;
        let final_ack;
        {
            let store = AdapterStore::open(&root).expect("store");
            let outbox = DiagnosticArtifactOutbox::open(store.clone()).expect("outbox");
            let execution = ExecutionOutbox::open(store).expect("execution outbox");
            retained = outbox.retain(&upload).expect("retain actual output");
            assert!(retained.deliveries.len() > 2, "open plus multiple chunks");
            assert_eq!(
                execution.pending().expect("pending frames").len(),
                retained.deliveries.len()
            );
            for delivery in &retained.deliveries {
                execution
                    .record_sent(&delivery.delivery_id)
                    .expect("record an in-flight sent attempt");
            }
            final_ack = ack(&upload, &retained);
            let mut gap_ack = final_ack.clone();
            gap_ack.ack_sequence = ExecutionAckSequence(1);
            gap_ack.error = Some(ExecutionPortError {
                code: ExecutionPortErrorCode::SequenceGap,
                message: "replay suffix".to_owned(),
                retryable: true,
            });
            gap_ack.replay_from_sequence = Some(ExecutionSequence(2));
            gap_ack.status = LeaseWriteStatus::Gap;
            let replay = outbox
                .apply_ack(&gap_ack)
                .expect("replay diagnostic suffix");
            assert!(matches!(
                replay,
                DiagnosticArtifactAckOutcome::Replay(ref deliveries) if deliveries.len() + 2 == retained.deliveries.len()
            ));
            let mut foreign = final_ack.clone();
            foreign.worker_session_id = WorkerSessionId("wsn_foreign_session".to_owned());
            assert_eq!(outbox.apply_ack(&foreign), Err(AdapterStoreError::Conflict));
            assert_eq!(
                outbox.apply_ack(&final_ack),
                Ok(DiagnosticArtifactAckOutcome::Accepted {
                    reference: retained.artifact.clone(),
                    authority: retained.authority.clone(),
                    run_key: retained.run_key.clone(),
                })
            );
            assert!(execution.pending().expect("compacted frames").is_empty());
        }
        {
            let store = AdapterStore::open(&root).expect("restart store");
            let outbox = DiagnosticArtifactOutbox::open(store).expect("restart outbox");
            let mut duplicate_ack = final_ack.clone();
            duplicate_ack.status = LeaseWriteStatus::Duplicate;
            assert_eq!(
                outbox.apply_ack(&duplicate_ack),
                Ok(DiagnosticArtifactAckOutcome::Duplicate {
                    reference: retained.artifact.clone(),
                    authority: retained.authority.clone(),
                    run_key: retained.run_key.clone(),
                })
            );
            assert_eq!(
                outbox
                    .accepted_references(&retained.authority)
                    .expect("references"),
                vec![retained.artifact]
            );
        }
        std::fs::remove_dir_all(root).expect("remove fixture");
    }
}
