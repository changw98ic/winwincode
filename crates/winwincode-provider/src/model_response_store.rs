// SPDX-License-Identifier: Apache-2.0

//! A complete Device response is validated once before its immutable replay index is sealed.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{SessionIdentity, WorkerSessionId};
use winwincode_execution_port::{
    generated::{ExecutionLeaseStamp, ExecutionPortMessage, ModelChunkMessage, ModelOpenMessage},
    typed_replay::{frame_from_message, stream_key_from_message},
};

use crate::DeviceProviderError;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelResponseBinding {
    lease: ExecutionLeaseStamp,
    worker_session_id: WorkerSessionId,
    session_identity: SessionIdentity,
}

impl ModelResponseBinding {
    fn from_chunk(chunk: &ModelChunkMessage) -> Self {
        Self {
            lease: chunk.lease.clone(),
            worker_session_id: chunk.worker_session_id.clone(),
            session_identity: chunk.session_identity.clone(),
        }
    }

    pub(crate) fn matches(&self, chunk: &ModelChunkMessage) -> bool {
        self.lease == chunk.lease
            && self.worker_session_id == chunk.worker_session_id
            && self.session_identity == chunk.session_identity
    }
}

pub(crate) struct ValidatedModelResponse {
    frames: Vec<(i64, String, String)>,
    pub(crate) digest: String,
    binding: ModelResponseBinding,
    highest_sequence: i64,
}

pub(crate) fn frame_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub(crate) fn canonical_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

pub(crate) fn validate_response_frame(
    chunk: &ModelChunkMessage,
) -> Result<(), DeviceProviderError> {
    frame_from_message(&ExecutionPortMessage::ModelChunkMessage(chunk.clone()))
        .map_err(|_| DeviceProviderError)?;
    if chunk.error.is_some() && !chunk.is_final {
        return Err(DeviceProviderError);
    }
    if let Some(payload) = &chunk.payload {
        let bytes = STANDARD
            .decode(&payload.data_base64)
            .map_err(|_| DeviceProviderError)?;
        if payload.content_type != "application/json"
            || frame_digest(&bytes) != payload.payload_digest.0
        {
            return Err(DeviceProviderError);
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        if !value.is_object() {
            return Err(DeviceProviderError);
        }
        if chunk.error.is_none() {
            let kind = value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .ok_or(DeviceProviderError)?;
            if chunk.is_final != matches!(kind, "completed" | "error") {
                return Err(DeviceProviderError);
            }
        }
    } else if !chunk.is_final {
        return Err(DeviceProviderError);
    }
    Ok(())
}

pub(crate) fn validate_model_response(
    exchange_id: &str,
    chunks: &[ModelChunkMessage],
    request_open: Option<&str>,
) -> Result<ValidatedModelResponse, DeviceProviderError> {
    let first = chunks.first().ok_or(DeviceProviderError)?;
    if exchange_id.trim().is_empty() || !chunks.last().is_some_and(|frame| frame.is_final) {
        return Err(DeviceProviderError);
    }
    let binding = ModelResponseBinding::from_chunk(first);
    if let Some(bytes) = request_open {
        let open: ModelOpenMessage = serde_json::from_str(bytes)?;
        if open.model_exchange_id.0 != exchange_id
            || open.lease != binding.lease
            || open.worker_session_id != binding.worker_session_id
            || open.session_identity != binding.session_identity
        {
            return Err(DeviceProviderError);
        }
    }
    let mut digest = Sha256::new();
    let mut frames = Vec::with_capacity(chunks.len());
    for (index, chunk) in chunks.iter().enumerate() {
        let sequence = i64::try_from(index + 1).map_err(|_| DeviceProviderError)?;
        if chunk.sequence.0 != sequence
            || chunk.model_exchange_id.0 != exchange_id
            || !binding.matches(chunk)
            || (chunk.is_final && index + 1 != chunks.len())
        {
            return Err(DeviceProviderError);
        }
        validate_response_frame(chunk)?;
        let bytes = serde_json::to_string(chunk)?;
        digest.update(
            u64::try_from(bytes.len())
                .map_err(|_| DeviceProviderError)?
                .to_le_bytes(),
        );
        digest.update(bytes.as_bytes());
        let fingerprint = frame_digest(bytes.as_bytes());
        frames.push((sequence, bytes, fingerprint));
    }
    Ok(ValidatedModelResponse {
        frames,
        digest: format!("sha256:{:x}", digest.finalize()),
        binding,
        highest_sequence: i64::try_from(chunks.len()).map_err(|_| DeviceProviderError)?,
    })
}

/// The caller owns the transaction which also writes the original private response.
pub(crate) fn insert_model_response_index(
    connection: &Connection,
    exchange_id: &str,
    response: &ValidatedModelResponse,
) -> Result<(), DeviceProviderError> {
    for (sequence, bytes, digest) in &response.frames {
        connection.execute(
            "INSERT INTO model_response_frame(exchange_id,sequence,frame_json,frame_digest) VALUES(?1,?2,?3,?4)",
            params![exchange_id, sequence, bytes, digest],
        )?;
    }
    connection.execute(
        "INSERT INTO model_response_exchange(exchange_id,worker_session_id,worker_instance_id,highest_sequence,response_digest,binding_json) VALUES(?1,?2,?3,?4,?5,?6)",
        params![exchange_id,response.binding.worker_session_id.0,response.binding.lease.worker_instance_id.0,
            response.highest_sequence,response.digest,serde_json::to_string(&response.binding)?],
    )?;
    Ok(())
}

pub(crate) fn migrate_model_response_index(
    connection: &Connection,
) -> Result<(), DeviceProviderError> {
    migrate_prepared_payload_digest(connection)?;
    connection.execute_batch(
        "DROP TRIGGER IF EXISTS model_response_insert;
         DROP TRIGGER IF EXISTS model_response_update;
         CREATE TABLE IF NOT EXISTS model_response_frame (
            exchange_id TEXT NOT NULL REFERENCES exchanges(exchange_id),
            sequence INTEGER NOT NULL CHECK(sequence>0), frame_json TEXT NOT NULL,
            frame_digest TEXT NOT NULL, PRIMARY KEY(exchange_id,sequence));
         CREATE TABLE IF NOT EXISTS model_response_exchange (
            exchange_id TEXT PRIMARY KEY REFERENCES exchanges(exchange_id),
            worker_session_id TEXT NOT NULL, worker_instance_id TEXT NOT NULL,
            highest_sequence INTEGER NOT NULL CHECK(highest_sequence>0),
            response_digest TEXT NOT NULL, binding_json TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS model_response_session_idx ON model_response_exchange(worker_session_id,exchange_id);
         CREATE INDEX IF NOT EXISTS model_response_instance_idx ON model_response_exchange(worker_instance_id,exchange_id);
         DELETE FROM model_response_frame;
         DELETE FROM model_response_exchange;",
    )?;
    let mut statement = connection.prepare(
        "SELECT exchange_id,chunks,request_open,digest FROM exchanges WHERE chunks IS NOT NULL ORDER BY exchange_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (exchange, original, request_open, original_digest) = row?;
        if request_open.as_deref().is_some_and(|bytes| {
            format!("{:x}", Sha256::digest(bytes.as_bytes())) != original_digest
        }) {
            return Err(DeviceProviderError);
        }
        let chunks: Vec<ModelChunkMessage> = serde_json::from_str(&original)?;
        let response = validate_model_response(&exchange, &chunks, request_open.as_deref())?;
        insert_model_response_index(connection, &exchange, &response)?;
    }
    // A raw source mutation invalidates the seal. Only the validated writer can
    // publish a replacement index, in the same transaction as the source bytes.
    connection.execute_batch(
        "CREATE TRIGGER model_response_update AFTER UPDATE OF exchange_id,digest,request_open,chunks ON exchanges
         WHEN NEW.chunks IS NOT OLD.chunks OR NEW.request_open IS NOT OLD.request_open OR NEW.digest IS NOT OLD.digest OR NEW.exchange_id IS NOT OLD.exchange_id BEGIN
            DELETE FROM model_response_frame WHERE exchange_id=OLD.exchange_id OR exchange_id=NEW.exchange_id;
            DELETE FROM model_response_exchange WHERE exchange_id=OLD.exchange_id OR exchange_id=NEW.exchange_id;
         END;
         CREATE TRIGGER IF NOT EXISTS model_response_delete AFTER DELETE ON exchanges BEGIN
            DELETE FROM model_response_frame WHERE exchange_id=OLD.exchange_id;
            DELETE FROM model_response_exchange WHERE exchange_id=OLD.exchange_id;
         END;
         CREATE TRIGGER IF NOT EXISTS model_response_frame_update AFTER UPDATE ON model_response_frame BEGIN
            DELETE FROM model_response_exchange WHERE exchange_id=OLD.exchange_id OR exchange_id=NEW.exchange_id;
         END;
         CREATE TRIGGER IF NOT EXISTS model_response_frame_delete AFTER DELETE ON model_response_frame BEGIN
            DELETE FROM model_response_exchange WHERE exchange_id=OLD.exchange_id;
         END;
         CREATE TRIGGER IF NOT EXISTS model_response_frame_insert AFTER INSERT ON model_response_frame BEGIN
            DELETE FROM model_response_exchange WHERE exchange_id=NEW.exchange_id;
         END;
         CREATE TRIGGER IF NOT EXISTS model_response_seal_immutable BEFORE UPDATE ON model_response_exchange BEGIN
            SELECT RAISE(ABORT,'sealed model response metadata is immutable');
         END;",
    )?;
    Ok(())
}

fn migrate_prepared_payload_digest(connection: &Connection) -> Result<(), DeviceProviderError> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('exchanges') WHERE name='prepared_payload_digest')",
        [], |row|row.get(0),
    )?;
    if !exists {
        connection
            .execute_batch("ALTER TABLE exchanges ADD COLUMN prepared_payload_digest TEXT;")?;
    }
    connection.execute_batch("DROP TRIGGER IF EXISTS prepared_model_payload_immutable;")?;
    let mut statement = connection.prepare("SELECT exchange_id,prepared_payload,prepared_payload_digest,request_open,digest FROM exchanges WHERE prepared_payload IS NOT NULL OR prepared_payload_digest IS NOT NULL")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<Vec<u8>>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (exchange, payload, original_digest, request_open, request_digest) = row?;
        let request_open = request_open.ok_or(DeviceProviderError)?;
        let open: ModelOpenMessage = serde_json::from_str(&request_open)?;
        if open.model_exchange_id.0 != exchange
            || format!("{:x}", Sha256::digest(request_open.as_bytes())) != request_digest
        {
            return Err(DeviceProviderError);
        }
        stream_key_from_message(&ExecutionPortMessage::ModelOpenMessage(open.clone()))
            .map_err(|_| DeviceProviderError)?;
        crate::device_model::validated_model_payload(&open)?;
        let digest = frame_digest(&payload.ok_or(DeviceProviderError)?);
        if original_digest
            .as_deref()
            .is_some_and(|original| original != digest)
        {
            return Err(DeviceProviderError);
        }
        connection.execute("UPDATE exchanges SET prepared_payload_digest=?1 WHERE exchange_id=?2 AND prepared_payload_digest IS NULL",params![digest,exchange])?;
    }
    connection.execute_batch(
        "CREATE TRIGGER prepared_model_payload_immutable BEFORE UPDATE OF exchange_id,digest,request_open,prepared_payload,prepared_payload_digest ON exchanges
         WHEN OLD.prepared_payload IS NOT NULL AND (NEW.exchange_id IS NOT OLD.exchange_id OR NEW.digest IS NOT OLD.digest OR NEW.request_open IS NOT OLD.request_open OR NEW.prepared_payload IS NOT OLD.prepared_payload OR NEW.prepared_payload_digest IS NOT OLD.prepared_payload_digest) BEGIN
            SELECT RAISE(ABORT,'prepared model payload is immutable');
         END;",
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "model_response_migration_tests.rs"]
mod tests;
