// SPDX-License-Identifier: Apache-2.0

//! Durable Worker-to-Control-Plane delivery ledger.
//!
//! This module stores the complete generated frame before the first transport
//! attempt. A successful send records only an attempt. Frames that require a
//! canonical response are replayed after restart until that response is
//! validated and applied.

use rusqlite::{OptionalExtension as _, Transaction, params};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use winwincode_domain::{ModelExchangeId, WorkerId, WorkerInstanceId};
use winwincode_execution_port::{
    generated::{
        ExecutionPortErrorCode, ExecutionPortMessage, JobOutcomeAckMessageStatus,
        JobOutcomeMessage, LeaseWriteStatus, ModelAckMessage, ModelOpenMessage,
        RuntimeEventMessage, WorkerHeartbeatAckMessageStatus,
        WorkerRegistrationResultMessageStatus,
    },
    runtime_replay::{RuntimeReplayAckReceipt, runtime_ack_stream_key, runtime_event_stream_key},
};

use crate::{DurableExecutionDelivery, store::AdapterStoreError};

const PENDING: &str = "pending";
const SENT_ATTEMPT: &str = "sent_attempt";
const HEARTBEAT_SEQUENCE_KEY_PREFIX: &str = "worker-heartbeat-sequence:";

/// Durable delivery operations over the adapter's one private `SQLite` store.
#[derive(Clone, Debug)]
pub(crate) struct ExecutionOutbox {
    store: crate::store::AdapterStore,
}

impl ExecutionOutbox {
    pub(crate) fn open(store: crate::store::AdapterStore) -> Result<Self, AdapterStoreError> {
        let outbox = Self { store };
        // A process may have stopped after send and before receipt. Only
        // response-bearing frames are made pending again. Transport-only
        // frames retain their sent-attempt record without being duplicated.
        outbox
            .store
            .lock()?
            .execute(
                "UPDATE execution_outbox SET state = ?1
                 WHERE acknowledgement_required = 1 AND state = ?2",
                params![PENDING, SENT_ATTEMPT],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(outbox)
    }

    pub(crate) fn retain(
        &self,
        message: &ExecutionPortMessage,
    ) -> Result<DurableExecutionDelivery, AdapterStoreError> {
        self.store
            .transaction(|transaction| Self::retain_in_transaction(transaction, message))
    }

    /// Keeps the first complete terminal frame after its transport row is acknowledged.
    /// The run, terminal snapshot and pending delivery commit in one transaction.
    pub(crate) fn retain_terminal_in_transaction(
        transaction: &Transaction<'_>,
        run_key: &str,
        outcome: &JobOutcomeMessage,
    ) -> Result<DurableExecutionDelivery, AdapterStoreError> {
        let saved = transaction.query_row(
            "SELECT frame_digest, frame_json FROM execution_terminal_outcome WHERE run_key = ?1",
            [run_key], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))
            .optional().map_err(|_| AdapterStoreError::Unavailable)?;
        // Upgrade an existing pending terminal before its ACK can compact it.
        let saved = if saved.is_some() {
            saved
        } else {
            transaction.query_row(
                "SELECT frame_digest, frame_json FROM execution_outbox WHERE delivery_id = ?1 AND family = ?2",
                params![outcome.message_id.0, Family::Outcome.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))
                .optional().map_err(|_| AdapterStoreError::Unavailable)?
        };
        let canonical = if let Some((frame_digest, bytes)) = saved {
            if digest(&bytes) != frame_digest {
                return Err(AdapterStoreError::Corrupt);
            }
            let message: ExecutionPortMessage =
                serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
            let ExecutionPortMessage::JobOutcomeMessage(original) = &message else {
                return Err(AdapterStoreError::Corrupt);
            };
            if original.lease != outcome.lease
                || original.worker_session_id != outcome.worker_session_id
                || original.session_identity != outcome.session_identity
                || original.outcome.codex_thread_id != outcome.outcome.codex_thread_id
                || original.message_id != outcome.message_id
            {
                return Err(AdapterStoreError::Conflict);
            }
            message
        } else {
            ExecutionPortMessage::JobOutcomeMessage(outcome.clone())
        };
        let delivery = Self::retain_in_transaction(transaction, &canonical)?;
        let bytes =
            serde_json::to_vec(&delivery.message).map_err(|_| AdapterStoreError::Corrupt)?;
        transaction.execute(
            "INSERT OR IGNORE INTO execution_terminal_outcome(run_key, frame_digest, frame_json) VALUES (?1, ?2, ?3)",
            params![run_key, digest(&bytes), bytes]).map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(delivery)
    }

    pub(crate) fn retain_in_transaction(
        transaction: &Transaction<'_>,
        message: &ExecutionPortMessage,
    ) -> Result<DurableExecutionDelivery, AdapterStoreError> {
        let frame = serde_json::to_vec(message).map_err(|_| AdapterStoreError::Corrupt)?;
        // A definitively refused auxiliary frame may have a durable bounded wire
        // projection. Keep its audited source bytes, and reuse that exact projection
        // for Core replay and runtime gap recovery as well as ordinary retries.
        let projected: Option<(String, Option<Vec<u8>>)> = transaction.query_row(
            "SELECT original_digest, projected_frame FROM execution_delivery_disposition WHERE delivery_id = ?1",
            [message_id(&frame)?], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional().map_err(|_| AdapterStoreError::Unavailable)?;
        if let Some((original_digest, Some(projected))) = projected
            && original_digest == digest(&frame)
            && projected != frame
        {
            let canonical =
                serde_json::from_slice(&projected).map_err(|_| AdapterStoreError::Corrupt)?;
            return Self::retain_in_transaction(transaction, &canonical);
        }
        let frame_digest = digest(&frame);
        let metadata = metadata(message, &frame)?;
        let existing = transaction
            .query_row(
                "SELECT delivery_id, frame_digest, frame_json FROM execution_outbox
                 WHERE delivery_id = ?1 OR (family = ?2 AND correlation_key = ?3)
                 ORDER BY position LIMIT 1",
                params![
                    metadata.delivery_id,
                    metadata.family,
                    metadata.correlation_key
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if let Some((delivery_id, existing_digest, existing_frame)) = existing {
            let same_original = existing_digest == frame_digest && existing_frame == frame;
            let same_terminal_facts = metadata.family == Family::Outcome.as_str()
                && same_outcome_facts(&existing_frame, &frame)?;
            // Worker message ids are process-local sequence values.  A
            // replacement Worker can therefore reconstruct the same logical
            // transport frame with a fresh timestamp (and, when the sequence
            // restarts, the same message id) before it has received the
            // predecessor's response.  Keep the predecessor's bytes as the
            // canonical replay and never replace them with the reconstruction.
            // The generated union tag and every authority/payload field remain
            // part of the comparison; only transport-attempt fields are
            // normalized here.
            let same_replay_facts = same_replay_facts(&existing_frame, &frame)?;
            if !same_original && !same_terminal_facts && !same_replay_facts {
                return Err(AdapterStoreError::Conflict);
            }
            return decode_delivery(delivery_id, &existing_frame);
        }

        transaction
            .execute(
                "INSERT INTO execution_outbox(
                   delivery_id, family, correlation_key, acknowledgement_required,
                   state, frame_digest, frame_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    metadata.delivery_id,
                    metadata.family,
                    metadata.correlation_key,
                    i64::from(metadata.acknowledgement_required),
                    PENDING,
                    frame_digest,
                    frame,
                ],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if let ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat) = message {
            if heartbeat.heartbeat_sequence.0 <= 0 {
                return Err(AdapterStoreError::Conflict);
            }
            let state_key =
                heartbeat_sequence_key(&heartbeat.worker_id, &heartbeat.worker_instance_id)?;
            transaction
                .execute(
                    "INSERT INTO worker_transport_state(state_key, sequence) VALUES (?1, ?2)
                     ON CONFLICT(state_key) DO UPDATE SET sequence = excluded.sequence
                     WHERE worker_transport_state.sequence < excluded.sequence",
                    params![state_key, heartbeat.heartbeat_sequence.0],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
        }
        Ok(DurableExecutionDelivery {
            delivery_id: metadata.delivery_id,
            message: message.clone(),
        })
    }

    pub(crate) fn pending(&self) -> Result<Vec<DurableExecutionDelivery>, AdapterStoreError> {
        let connection = self.store.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT delivery_id, frame_json FROM execution_outbox
                 WHERE state = ?1 OR acknowledgement_required = 1 ORDER BY position",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let rows = statement
            .query_map(params![PENDING], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|_| AdapterStoreError::Unavailable)?;
        rows.map(|row| {
            let (delivery_id, frame) = row.map_err(|_| AdapterStoreError::Unavailable)?;
            decode_delivery(delivery_id, &frame)
        })
        .collect()
    }

    /// Keeps the original interaction deadline across execution lease renewals.
    pub(crate) fn retained_interaction(
        &self,
        delivery_id: &str,
    ) -> Result<Option<ExecutionPortMessage>, AdapterStoreError> {
        let saved = self
            .store
            .lock()?
            .query_row(
                "SELECT frame_digest, frame_json FROM execution_outbox WHERE delivery_id = ?1",
                [delivery_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let Some((frame_digest, frame)) = saved else {
            return Ok(None);
        };
        if digest(&frame) != frame_digest {
            return Err(AdapterStoreError::Corrupt);
        }
        let message = decode_delivery(delivery_id.to_owned(), &frame)?.message;
        if !matches!(
            message,
            ExecutionPortMessage::ApprovalRequestMessage(_)
                | ExecutionPortMessage::InputRequestMessage(_)
        ) {
            return Err(AdapterStoreError::Conflict);
        }
        Ok(Some(message))
    }

    /// A local deadline response settles transport replay, without inventing a CP receipt.
    pub(crate) fn settle_interaction_deadline(
        &self,
        request: &ExecutionPortMessage,
    ) -> Result<(), AdapterStoreError> {
        if !matches!(
            request,
            ExecutionPortMessage::ApprovalRequestMessage(_)
                | ExecutionPortMessage::InputRequestMessage(_)
        ) {
            return Err(AdapterStoreError::Conflict);
        }
        let frame = serde_json::to_vec(request).map_err(|_| AdapterStoreError::Corrupt)?;
        let changed = self
            .store
            .lock()?
            .execute(
                "UPDATE execution_outbox SET state = ?1, acknowledgement_required = 0
             WHERE delivery_id = ?2 AND frame_digest = ?3 AND frame_json = ?4",
                params![SENT_ATTEMPT, message_id(&frame)?, digest(&frame), frame],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if changed != 1 {
            return Err(AdapterStoreError::Conflict);
        }
        Ok(())
    }

    pub(crate) fn pending_batch(
        &self,
        after_delivery: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DurableExecutionDelivery>, AdapterStoreError> {
        let connection = self.store.lock()?;
        let mut statement = connection.prepare(
            "SELECT delivery_id, frame_json FROM execution_outbox
             WHERE (state = ?1 OR acknowledgement_required = 1)
               AND position > COALESCE((SELECT position FROM execution_outbox WHERE delivery_id = ?2), 0)
             ORDER BY position LIMIT ?3",
        ).map_err(|_| AdapterStoreError::Unavailable)?;
        let rows = statement
            .query_map(
                params![
                    PENDING,
                    after_delivery,
                    i64::try_from(limit).map_err(|_| AdapterStoreError::Conflict)?
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        rows.map(|row| {
            let (id, frame) = row.map_err(|_| AdapterStoreError::Unavailable)?;
            decode_delivery(id, &frame)
        })
        .collect()
    }

    /// Returns the highest numeric Worker message id retained in the outbox.
    ///
    /// Transport-only frames remain durable after a successful send so their
    /// identity can be audited, but they are intentionally omitted from
    /// [`Self::pending`]. A replacement Worker still has to advance past
    /// those ids before creating a new transport frame, otherwise its first
    /// post-restart dispatch result can collide with the predecessor's frame
    /// and be treated as a changed replay.
    pub(crate) fn highest_numeric_message_sequence(&self) -> Result<u64, AdapterStoreError> {
        let connection = self.store.lock()?;
        let mut statement = connection
            .prepare("SELECT frame_json FROM execution_outbox ORDER BY position")
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let rows = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let mut highest = 0;
        for row in rows {
            let frame = row.map_err(|_| AdapterStoreError::Unavailable)?;
            let value: Value =
                serde_json::from_slice(&frame).map_err(|_| AdapterStoreError::Corrupt)?;
            let Some(message_id) = value.get("messageId").and_then(Value::as_str) else {
                return Err(AdapterStoreError::Corrupt);
            };
            if let Some(sequence) = message_id
                .strip_prefix("xmsg_")
                .and_then(|suffix| suffix.parse::<u64>().ok())
            {
                highest = highest.max(sequence);
            }
        }
        Ok(highest)
    }

    /// Returns the highest heartbeat sequence committed with its canonical
    /// outbound frame. The cursor remains after an accepted acknowledgement
    /// compacts that frame, allowing the same Worker instance to continue
    /// monotonically after an operating-system process restart.
    pub(crate) fn heartbeat_sequence_highwater(
        &self,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
    ) -> Result<i64, AdapterStoreError> {
        let connection = self.store.lock()?;
        stored_heartbeat_sequence(&connection, worker_id, worker_instance_id)
    }

    pub(crate) fn record_sent(&self, delivery_id: &str) -> Result<(), AdapterStoreError> {
        let connection = self.store.lock()?;
        let changed = connection
            .execute(
                "UPDATE execution_outbox SET state = ?1
                 WHERE delivery_id = ?2 AND state IN (?1, ?3)",
                params![SENT_ATTEMPT, delivery_id, PENDING],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if changed != 1 {
            return Err(AdapterStoreError::Conflict);
        }
        Ok(())
    }

    pub(crate) fn requeue(&self, delivery_id: &str) -> Result<(), AdapterStoreError> {
        let changed = self
            .store
            .lock()?
            .execute(
                "UPDATE execution_outbox SET state=?2 WHERE delivery_id=?1
                 AND NOT (family IN ('approval_request', 'input_request') AND acknowledgement_required=0)
                 AND NOT EXISTS (SELECT 1 FROM execution_delivery_disposition d WHERE d.delivery_id=?1 AND d.rejected=1)",
                params![delivery_id, PENDING],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if changed != 1 {
            return Err(AdapterStoreError::Conflict);
        }
        Ok(())
    }

    pub(crate) fn degrade_auxiliary(
        &self,
        delivery_id: &str,
    ) -> Result<Option<DurableExecutionDelivery>, AdapterStoreError> {
        self.store.transaction(|transaction| {
            let already: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM execution_delivery_disposition WHERE delivery_id = ?1)",
                [delivery_id], |row| row.get(0)).map_err(|_| AdapterStoreError::Unavailable)?;
            if already { return Ok(None); }
            let bytes: Vec<u8> = transaction.query_row(
                "SELECT frame_json FROM execution_outbox WHERE delivery_id = ?1",
                [delivery_id], |row| row.get(0)).map_err(|_| AdapterStoreError::Conflict)?;
            let mut message: ExecutionPortMessage = serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt)?;
            let ExecutionPortMessage::RuntimeEventMessage(event) = &mut message else { return Ok(None); };
            if event.event.category != winwincode_execution_port::generated::ExecutionEventCategory::Usage {
                return Ok(None);
            }
            // Preserve stream identity and sequence, so dropping a report does not
            // leave a permanent runtime replay gap. No approval/verification fact is removed.
            event.event.payload = None;
            event.event.summary = "optional performance report unavailable".into();
            let projected = serde_json::to_vec(&message).map_err(|_| AdapterStoreError::Corrupt)?;
            transaction.execute(
                "INSERT INTO execution_delivery_disposition(delivery_id, original_digest, original_frame, projected_frame) VALUES (?1,?2,?3,?4)",
                params![delivery_id, digest(&bytes), bytes, projected]).map_err(|_| AdapterStoreError::Unavailable)?;
            transaction.execute("UPDATE execution_outbox SET frame_json=?2,frame_digest=?3,state=?4 WHERE delivery_id=?1",
                params![delivery_id, projected, digest(&projected), PENDING]).map_err(|_| AdapterStoreError::Unavailable)?;
            Ok(Some(DurableExecutionDelivery { delivery_id: delivery_id.to_owned(), message }))
        })
    }

    pub(crate) fn record_rejected(&self, delivery_id: &str) -> Result<(), AdapterStoreError> {
        self.store
            .transaction(|transaction| record_rejected_in_transaction(transaction, delivery_id))
    }

    pub(crate) fn is_rejected(&self, delivery_id: &str) -> Result<bool, AdapterStoreError> {
        self.store
            .lock()?
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM execution_delivery_disposition WHERE delivery_id=?1 AND rejected=1)",
                [delivery_id],
                |row| row.get(0),
            )
            .map_err(|_| AdapterStoreError::Unavailable)
    }

    pub(crate) fn has_rejected_terminal(
        &self,
        worker_id: &WorkerId,
        instance_id: &WorkerInstanceId,
    ) -> Result<bool, AdapterStoreError> {
        self.store
            .lock()?
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM execution_delivery_disposition WHERE rejected=1 AND json_extract(original_frame,'$.kind')='job.outcome'
                 AND json_extract(original_frame,'$.lease.workerId')=?1 AND json_extract(original_frame,'$.lease.workerInstanceId')=?2)",
                params![worker_id.0, instance_id.0],
                |row| row.get(0),
            )
            .map_err(|_| AdapterStoreError::Unavailable)
    }

    pub(crate) fn has_rejected_job(
        &self,
        lease: &winwincode_execution_port::generated::ExecutionLeaseStamp,
    ) -> Result<bool, AdapterStoreError> {
        self.store.lock()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM execution_delivery_disposition WHERE rejected=1 AND json_extract(original_frame,'$.kind')!='job.outcome'
              AND json_extract(original_frame,'$.lease.jobId')=?1
              AND json_extract(original_frame,'$.lease.attempt')=?2
              AND json_extract(original_frame,'$.lease.leaseId')=?3
              AND json_extract(original_frame,'$.lease.fencingToken')=?4
              AND json_extract(original_frame,'$.lease.workerId')=?5
              AND json_extract(original_frame,'$.lease.workerInstanceId')=?6
              AND json_extract(original_frame,'$.lease.issuedAt')=?7)",
            params![lease.job_id.0,lease.attempt,lease.lease_id.0,lease.fencing_token.0,lease.worker_id.0,lease.worker_instance_id.0,lease.issued_at.0],
            |row| row.get(0)).map_err(|_| AdapterStoreError::Unavailable)
    }

    /// Loads the exact retained open frame for one durable model exchange.
    pub(crate) fn retained_model_open(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ModelOpenMessage>, AdapterStoreError> {
        Self::read_model_open(&self.store, model_exchange_id)
    }

    /// Reads original exchange authority without reopening or mutating the outbox.
    pub(crate) fn read_model_open(
        store: &crate::store::AdapterStore,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ModelOpenMessage>, AdapterStoreError> {
        let connection = store.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT frame_json FROM execution_outbox
                 WHERE family = ?1 ORDER BY position",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let rows = statement
            .query_map(params![Family::ModelOpen.as_str()], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let mut retained = None;
        for row in rows {
            let frame = row.map_err(|_| AdapterStoreError::Unavailable)?;
            let message: ExecutionPortMessage =
                serde_json::from_slice(&frame).map_err(|_| AdapterStoreError::Corrupt)?;
            let ExecutionPortMessage::ModelOpenMessage(open) = message else {
                return Err(AdapterStoreError::Corrupt);
            };
            if open.model_exchange_id == *model_exchange_id && retained.replace(open).is_some() {
                return Err(AdapterStoreError::Corrupt);
            }
        }
        Ok(retained)
    }

    /// Every model role uses the exact retained request and the same durable
    /// cancellation proof, including before its cancellation effect is dispatched.
    pub(crate) fn model_start_request_allowed(
        store: &crate::store::AdapterStore,
        open: &ModelOpenMessage,
    ) -> Result<bool, AdapterStoreError> {
        if Self::read_model_open(store, &open.model_exchange_id)?.as_ref() != Some(open) {
            return Ok(false);
        }
        let key = correlation(&(
            &open.model_exchange_id,
            &open.lease,
            &open.worker_session_id,
            &open.session_identity,
        ))?;
        let cancelled: bool = store.lock()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM execution_response_receipt WHERE family = ?1 AND correlation_key = ?2)",
            params![Family::ModelOpen.as_str(), format!("{key}.cancelled")], |row| row.get(0))
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(!cancelled)
    }

    /// Applies an already domain-validated response to its exact retained request.
    pub(crate) fn acknowledge_response(
        &self,
        acknowledgement: &ExecutionPortMessage,
    ) -> Result<(), AdapterStoreError> {
        if let ExecutionPortMessage::RuntimeAckMessage(_) = acknowledgement {
            return Err(AdapterStoreError::Conflict);
        }
        if retryable_heartbeat_gap(acknowledgement) {
            let (family, key) =
                response_target(acknowledgement)?.ok_or(AdapterStoreError::Conflict)?;
            // A gap consumes a control receipt, not the retained request. Only
            // this exact request (or its applied positive proof) authorizes it.
            let known: bool = self.store.lock()?.query_row(
                "SELECT EXISTS(SELECT 1 FROM execution_outbox WHERE family=?1 AND correlation_key=?2)
                 OR EXISTS(SELECT 1 FROM execution_response_receipt WHERE family=?1 AND correlation_key=?2)",
                params![family.as_str(), key], |row| row.get(0))
                .map_err(|_| AdapterStoreError::Unavailable)?;
            return if known {
                Ok(())
            } else {
                Err(AdapterStoreError::Conflict)
            };
        }
        if !accepted_response(acknowledgement) {
            return Err(AdapterStoreError::Conflict);
        }
        let Some((family, correlation_key)) = response_target(acknowledgement)? else {
            return Err(AdapterStoreError::Conflict);
        };
        let proof_key = response_proof_key(acknowledgement, &correlation_key);
        let response_digest = response_replay_digest(acknowledgement)?;
        self.store.transaction(|transaction| {
            let proof: Option<String> = transaction.query_row(
                "SELECT response_digest FROM execution_response_receipt WHERE family = ?1 AND correlation_key = ?2",
                params![family.as_str(), proof_key], |row| row.get(0))
                .optional().map_err(|_| AdapterStoreError::Unavailable)?;
            if proof.as_ref().is_some_and(|digest| digest != &response_digest) {
                return Err(AdapterStoreError::Conflict);
            }
            let changed = if matches!(family, Family::ModelOpen) {
                transaction.execute(
                    "UPDATE execution_outbox SET acknowledgement_required = 0, state = ?3
                     WHERE family = ?1 AND correlation_key = ?2 AND acknowledgement_required = 1",
                    params![family.as_str(), correlation_key, SENT_ATTEMPT])
            } else {
                transaction.execute(
                    "DELETE FROM execution_outbox WHERE family = ?1 AND correlation_key = ?2
                     AND acknowledgement_required = 1", params![family.as_str(), correlation_key])
            }.map_err(|_| AdapterStoreError::Unavailable)?;
            if changed == 1 {
                save_response_proof(transaction, family, &proof_key, &response_digest)?;
                return Ok(());
            }
            if proof.is_some() {
                return Ok(());
            }
            if matches!(acknowledgement, ExecutionPortMessage::ModelAckMessage(ack) if canonical_terminal_model_ack(ack)) {
                // A first chunk already clears acknowledgement_required, but the
                // exact open remains authority for a later Worker cancellation.
                let retained: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM execution_outbox WHERE family=?1 AND correlation_key=?2)",
                    params![family.as_str(), correlation_key], |row| row.get(0))
                    .map_err(|_| AdapterStoreError::Unavailable)?;
                if retained {
                    save_response_proof(transaction, family, &proof_key, &response_digest)?;
                    return Ok(());
                }
            }
            if matches!(acknowledgement, ExecutionPortMessage::ModelAckMessage(ack) if public_model_ack(ack)) {
                return Ok(());
            }
            if let ExecutionPortMessage::WorkerHeartbeatAckMessage(ack) = acknowledgement
                && ack.heartbeat_sequence.0 > 0
                && ack.heartbeat_sequence.0 <= stored_heartbeat_sequence(transaction, &ack.worker_id, &ack.worker_instance_id)? {
                return Ok(());
            }
            Err(AdapterStoreError::Conflict)
        })
    }

    /// Records proof only after the adapter has durably applied the exact domain response.
    /// This also repairs the crash/upgrade boundary where the request was already compacted.
    pub(crate) fn record_applied_response(
        &self,
        response: &ExecutionPortMessage,
    ) -> Result<(), AdapterStoreError> {
        if !accepted_response(response) {
            return Err(AdapterStoreError::Conflict);
        }
        let (family, key) = response_target(response)?.ok_or(AdapterStoreError::Conflict)?;
        let key = response_proof_key(response, &key);
        let digest = response_replay_digest(response)?;
        self.store
            .transaction(|transaction| save_response_proof(transaction, family, &key, &digest))
    }

    /// Compacts accepted runtime rows or makes exact gap frames pending again.
    pub(crate) fn apply_runtime_ack(
        &self,
        acknowledgement: &winwincode_execution_port::generated::RuntimeAckMessage,
        receipt: &RuntimeReplayAckReceipt,
    ) -> Result<Vec<DurableExecutionDelivery>, AdapterStoreError> {
        if acknowledgement.status != receipt.status {
            return Err(AdapterStoreError::Conflict);
        }
        match receipt.status {
            LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate => {
                let ack_sequence = u64::try_from(receipt.ack_sequence.0)
                    .map_err(|_| AdapterStoreError::Corrupt)?;
                self.store.transaction(|transaction| {
                    let rows = runtime_rows(transaction)?;
                    let stream = runtime_ack_stream_key(acknowledgement);
                    for row in rows {
                        if runtime_event_stream_key(&row.event) == stream
                            && event_sequence(&row.event)? <= ack_sequence
                        {
                            transaction
                                .execute(
                                    "DELETE FROM execution_outbox WHERE delivery_id = ?1",
                                    params![row.delivery_id],
                                )
                                .map_err(|_| AdapterStoreError::Unavailable)?;
                        }
                    }
                    Ok(Vec::new())
                })
            }
            LeaseWriteStatus::Gap => {
                let replay = receipt.replay.as_ref().ok_or(AdapterStoreError::Corrupt)?;
                self.requeue_runtime_events(&replay.events)
            }
            LeaseWriteStatus::RejectedConflict
            | LeaseWriteStatus::RejectedExpiredLease
            | LeaseWriteStatus::RejectedStaleFencingToken
            | LeaseWriteStatus::RejectedWorkerInstance => self.store.transaction(|transaction| {
                // The replay responder has authenticated this stream against
                // current authority. Keep the refused evidence, then let the
                // Worker settle its job through the existing durable failure path.
                let bytes =
                    serde_json::to_vec(acknowledgement).map_err(|_| AdapterStoreError::Corrupt)?;
                save_response_proof(
                    transaction,
                    Family::Runtime,
                    &format!("rejected:{}", acknowledgement.message_id.0),
                    &digest(&bytes),
                )?;
                let stream = runtime_ack_stream_key(acknowledgement);
                for row in runtime_rows(transaction)? {
                    if runtime_event_stream_key(&row.event) == stream {
                        record_rejected_in_transaction(transaction, &row.delivery_id)?;
                    }
                }
                Ok(Vec::new())
            }),
        }
    }

    /// Retains and returns only the requested original runtime frames.
    pub(crate) fn requeue_runtime_events(
        &self,
        events: &[RuntimeEventMessage],
    ) -> Result<Vec<DurableExecutionDelivery>, AdapterStoreError> {
        self.store.transaction(|transaction| {
            let mut deliveries = Vec::with_capacity(events.len());
            for event in events {
                let message = ExecutionPortMessage::RuntimeEventMessage(event.clone());
                let delivery = Self::retain_in_transaction(transaction, &message)?;
                let rejected: bool = transaction
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM execution_delivery_disposition WHERE delivery_id = ?1 AND rejected=1)",
                        [&delivery.delivery_id],
                        |row| row.get(0),
                    )
                    .map_err(|_| AdapterStoreError::Unavailable)?;
                if rejected {
                    continue;
                }
                transaction
                    .execute(
                        "UPDATE execution_outbox SET state = ?1 WHERE delivery_id = ?2",
                        params![PENDING, delivery.delivery_id],
                    )
                    .map_err(|_| AdapterStoreError::Unavailable)?;
                deliveries.push(delivery);
            }
            Ok(deliveries)
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Family {
    Runtime,
    Outcome,
    Artifact,
    ModelOpen,
    ModelPublicFrame,
    ActionRequest,
    ApprovalRequest,
    InputRequest,
    SessionBinding,
    WorkerRegister,
    WorkerHeartbeat,
    Transport,
}

impl Family {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Outcome => "outcome",
            Self::Artifact => "artifact",
            Self::ModelOpen => "model_open",
            Self::ModelPublicFrame => "model_public_frame",
            Self::ActionRequest => "action_request",
            Self::ApprovalRequest => "approval_request",
            Self::InputRequest => "input_request",
            Self::SessionBinding => "session_binding",
            Self::WorkerRegister => "worker_register",
            Self::WorkerHeartbeat => "worker_heartbeat",
            Self::Transport => "transport",
        }
    }
}

struct Metadata {
    delivery_id: String,
    family: &'static str,
    correlation_key: String,
    acknowledgement_required: bool,
}

#[allow(clippy::too_many_lines)]
fn metadata(message: &ExecutionPortMessage, frame: &[u8]) -> Result<Metadata, AdapterStoreError> {
    let delivery_id = message_id(frame)?;
    let (family, key) = match message {
        ExecutionPortMessage::RuntimeEventMessage(message) => (
            Family::Runtime,
            correlation(&(
                runtime_event_stream_key(message).as_str(),
                event_sequence(message)?,
            ))?,
        ),
        ExecutionPortMessage::JobOutcomeMessage(message) => {
            (Family::Outcome, correlation(&job_authority(message))?)
        }
        ExecutionPortMessage::ArtifactOpenMessage(message) => (
            Family::Artifact,
            correlation(&(
                &message.artifact.artifact_id,
                0_i64,
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
            ))?,
        ),
        ExecutionPortMessage::ArtifactChunkMessage(message) => (
            Family::Artifact,
            correlation(&(
                &message.artifact_id,
                &message.sequence,
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
            ))?,
        ),
        ExecutionPortMessage::ModelChunkMessage(message) => (
            Family::ModelPublicFrame,
            correlation(&(
                &message.model_exchange_id,
                &message.sequence,
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
            ))?,
        ),
        ExecutionPortMessage::ModelOpenMessage(message) => (
            Family::ModelOpen,
            correlation(&(
                &message.model_exchange_id,
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
            ))?,
        ),
        // A normal model acknowledgement is a Worker->Provider progress
        // frame, not a request which receives another protocol response. It
        // still gets retained before the send, but once the transport send is
        // recorded it must not remain forever in the response-replay set.
        // Cancellation is the one terminal ModelAck which deliberately
        // acknowledges the retained ModelOpen exchange.
        ExecutionPortMessage::ModelAckMessage(message)
            if !canonical_terminal_model_ack(message) =>
        {
            (Family::Transport, correlation(&delivery_id)?)
        }
        ExecutionPortMessage::ActionEnforcementRequestMessage(message) => (
            Family::ActionRequest,
            correlation(&(
                &message.request_id,
                &message.job_id,
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
            ))?,
        ),
        ExecutionPortMessage::ApprovalRequestMessage(message) => (
            Family::ApprovalRequest,
            correlation(&(
                &message.approval_id,
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
            ))?,
        ),
        ExecutionPortMessage::InputRequestMessage(message) => (
            Family::InputRequest,
            correlation(&(
                &message.input_request_id,
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
            ))?,
        ),
        // SessionBinding is a durable Worker fact, but it has no response
        // frame of its own.  Its semantic identity must survive a Worker
        // restart: messageId and boundAt are transport-attempt fields, while
        // the lease/session/thread tuple identifies the exact binding.  Using
        // that tuple here lets a replacement Worker recover the predecessor's
        // canonical bytes instead of creating a second two-phase CP commit.
        ExecutionPortMessage::SessionBindingMessage(message) => (
            Family::SessionBinding,
            correlation(&(
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
                &message.codex_thread_id,
            ))?,
        ),
        ExecutionPortMessage::WorkerRegisterMessage(message) => (
            Family::WorkerRegister,
            correlation(&(
                &message.request_id,
                &message.worker_id,
                &message.worker_instance_id,
            ))?,
        ),
        ExecutionPortMessage::WorkerHeartbeatMessage(message) => (
            Family::WorkerHeartbeat,
            correlation(&(
                &message.heartbeat_sequence,
                &message.worker_id,
                &message.worker_instance_id,
            ))?,
        ),
        _ => (Family::Transport, correlation(&delivery_id)?),
    };
    Ok(Metadata {
        delivery_id,
        family: family.as_str(),
        correlation_key: key,
        acknowledgement_required: !matches!(family, Family::Transport | Family::SessionBinding),
    })
}

fn response_target(
    message: &ExecutionPortMessage,
) -> Result<Option<(Family, String)>, AdapterStoreError> {
    let target = match message {
        ExecutionPortMessage::JobOutcomeAckMessage(message) => Some((
            Family::Outcome,
            correlation(&(
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
            ))?,
        )),
        ExecutionPortMessage::ModelChunkMessage(message) => Some((
            Family::ModelOpen,
            correlation(&(
                &message.model_exchange_id,
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
            ))?,
        )),
        ExecutionPortMessage::ModelAckMessage(message) if public_model_ack(message) => Some((
            Family::ModelPublicFrame,
            correlation(&(
                &message.model_exchange_id,
                &message.ack_sequence,
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
            ))?,
        )),
        ExecutionPortMessage::ModelAckMessage(message) if canonical_terminal_model_ack(message) => {
            Some((
                Family::ModelOpen,
                correlation(&(
                    &message.model_exchange_id,
                    &message.lease,
                    &message.worker_session_id,
                    &message.session_identity,
                ))?,
            ))
        }
        ExecutionPortMessage::ActionEnforcementReceiptMessage(message) => Some((
            Family::ActionRequest,
            correlation(&(
                &message.request_id,
                &message.job_id,
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
            ))?,
        )),
        ExecutionPortMessage::ApprovalDecisionMessage(message) => Some((
            Family::ApprovalRequest,
            correlation(&(
                &message.approval_id,
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
            ))?,
        )),
        ExecutionPortMessage::InputResponseMessage(message) => Some((
            Family::InputRequest,
            correlation(&(
                &message.input_request_id,
                &message.lease,
                &message.worker_session_id,
                &message.session_identity,
            ))?,
        )),
        ExecutionPortMessage::WorkerRegistrationResultMessage(message) => Some((
            Family::WorkerRegister,
            correlation(&(
                &message.request_id,
                &message.worker_id,
                &message.worker_instance_id,
            ))?,
        )),
        ExecutionPortMessage::WorkerHeartbeatAckMessage(message) => Some((
            Family::WorkerHeartbeat,
            correlation(&(
                &message.heartbeat_sequence,
                &message.worker_id,
                &message.worker_instance_id,
            ))?,
        )),
        _ => None,
    };
    Ok(target)
}

fn response_replay_digest(message: &ExecutionPortMessage) -> Result<String, AdapterStoreError> {
    let mut value = serde_json::to_value(message).map_err(|_| AdapterStoreError::Corrupt)?;
    let object = value.as_object_mut().ok_or(AdapterStoreError::Corrupt)?;
    object.remove("messageId");
    object.remove("sentAt");
    if matches!(
        message,
        ExecutionPortMessage::WorkerRegistrationResultMessage(_)
            | ExecutionPortMessage::WorkerHeartbeatAckMessage(_)
    ) {
        // The Control Plane rebuilds these receipts with its current clock on
        // a duplicate request. That observation is not the applied result.
        object.remove("serverTime");
    }
    if matches!(
        message,
        ExecutionPortMessage::JobOutcomeAckMessage(_)
            | ExecutionPortMessage::WorkerRegistrationResultMessage(_)
            | ExecutionPortMessage::WorkerHeartbeatAckMessage(_)
    ) && object.get("status").and_then(Value::as_str) == Some("duplicate")
    {
        object.insert("status".into(), Value::String("accepted".into()));
    }
    if matches!(message, ExecutionPortMessage::ModelAckMessage(ack) if canonical_terminal_model_ack(ack))
    {
        // Existing Observer wording is the same typed cancellation fact. Preserve
        // wire bytes, while normalising only these two accepted legacy descriptions.
        object
            .get_mut("error")
            .and_then(Value::as_object_mut)
            .ok_or(AdapterStoreError::Corrupt)?
            .insert(
                "message".into(),
                Value::String("model exchange cancelled by Worker".into()),
            );
    }
    serde_json::to_vec(&value)
        .map(|bytes| digest(&bytes))
        .map_err(|_| AdapterStoreError::Corrupt)
}

fn response_proof_key(message: &ExecutionPortMessage, correlation_key: &str) -> String {
    if matches!(message, ExecutionPortMessage::ModelAckMessage(ack) if canonical_terminal_model_ack(ack))
    {
        format!("{correlation_key}.cancelled")
    } else {
        correlation_key.to_owned()
    }
}

fn save_response_proof(
    transaction: &Transaction<'_>,
    family: Family,
    correlation_key: &str,
    response_digest: &str,
) -> Result<(), AdapterStoreError> {
    let previous: Option<String> = transaction.query_row(
        "SELECT response_digest FROM execution_response_receipt WHERE family = ?1 AND correlation_key = ?2",
        params![family.as_str(), correlation_key], |row| row.get(0))
        .optional().map_err(|_| AdapterStoreError::Unavailable)?;
    if previous
        .as_deref()
        .is_some_and(|previous| previous != response_digest)
    {
        return Err(AdapterStoreError::Conflict);
    }
    transaction.execute(
        "INSERT OR IGNORE INTO execution_response_receipt(family, correlation_key, response_digest) VALUES (?1, ?2, ?3)",
        params![family.as_str(), correlation_key, response_digest])
        .map_err(|_| AdapterStoreError::Unavailable)?;
    Ok(())
}

fn retryable_heartbeat_gap(message: &ExecutionPortMessage) -> bool {
    matches!(message, ExecutionPortMessage::WorkerHeartbeatAckMessage(ack)
        if ack.status == WorkerHeartbeatAckMessageStatus::RejectedWorkerInstance
            && ack.error.as_ref().is_some_and(|error|
                error.code == ExecutionPortErrorCode::SequenceGap && error.retryable))
}

fn accepted_response(message: &ExecutionPortMessage) -> bool {
    match message {
        ExecutionPortMessage::JobOutcomeAckMessage(message) => matches!(
            message.status,
            JobOutcomeAckMessageStatus::Accepted | JobOutcomeAckMessageStatus::Duplicate
        ),
        ExecutionPortMessage::WorkerRegistrationResultMessage(message) => matches!(
            message.status,
            WorkerRegistrationResultMessageStatus::Accepted
                | WorkerRegistrationResultMessageStatus::Duplicate
        ),
        ExecutionPortMessage::WorkerHeartbeatAckMessage(message) => {
            message.error.is_none()
                && matches!(
                    message.status,
                    WorkerHeartbeatAckMessageStatus::Accepted
                        | WorkerHeartbeatAckMessageStatus::Duplicate
                )
        }
        ExecutionPortMessage::ModelChunkMessage(_)
        | ExecutionPortMessage::ActionEnforcementReceiptMessage(_)
        | ExecutionPortMessage::ApprovalDecisionMessage(_)
        | ExecutionPortMessage::InputResponseMessage(_) => true,
        ExecutionPortMessage::ModelAckMessage(message) => {
            canonical_terminal_model_ack(message) || public_model_ack(message)
        }
        _ => false,
    }
}

fn public_model_ack(message: &ModelAckMessage) -> bool {
    matches!(
        message.status,
        LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
    ) && message.ack_sequence.0 > 0
        && message.error.is_none()
        && message.replay_from_sequence.is_none()
}

fn canonical_terminal_model_ack(message: &ModelAckMessage) -> bool {
    message.status == LeaseWriteStatus::RejectedConflict
        && message.ack_sequence.0 == 0
        && message.replay_from_sequence.is_none()
        && message.error.as_ref().is_some_and(|error| {
            error.code == ExecutionPortErrorCode::Cancelled
                && matches!(
                    error.message.as_str(),
                    "model exchange cancelled by Worker"
                        | "Observer model exchange cancelled by Worker"
                )
                && !error.retryable
        })
}

fn job_authority(
    message: &JobOutcomeMessage,
) -> (
    &winwincode_execution_port::generated::ExecutionLeaseStamp,
    &winwincode_domain::WorkerSessionId,
    &winwincode_domain::SessionIdentity,
) {
    (
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
    )
}

fn correlation(value: &impl Serialize) -> Result<String, AdapterStoreError> {
    let bytes = serde_json::to_vec(value).map_err(|_| AdapterStoreError::Corrupt)?;
    Ok(digest(&bytes))
}

fn heartbeat_sequence_key(
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> Result<String, AdapterStoreError> {
    Ok(format!(
        "{HEARTBEAT_SEQUENCE_KEY_PREFIX}{}",
        correlation(&(worker_id, worker_instance_id))?
    ))
}

fn stored_heartbeat_sequence(
    connection: &rusqlite::Connection,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> Result<i64, AdapterStoreError> {
    let state_key = heartbeat_sequence_key(worker_id, worker_instance_id)?;
    let sequence = connection
        .query_row(
            "SELECT sequence FROM worker_transport_state WHERE state_key = ?1",
            params![state_key],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?
        .unwrap_or(0);
    if sequence < 0 {
        return Err(AdapterStoreError::Corrupt);
    }
    Ok(sequence)
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn message_id(frame: &[u8]) -> Result<String, AdapterStoreError> {
    let value: Value = serde_json::from_slice(frame).map_err(|_| AdapterStoreError::Corrupt)?;
    value
        .get("messageId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(AdapterStoreError::Corrupt)
}

fn same_outcome_facts(first: &[u8], second: &[u8]) -> Result<bool, AdapterStoreError> {
    fn normalized(bytes: &[u8]) -> Result<Value, AdapterStoreError> {
        let mut value: Value =
            serde_json::from_slice(bytes).map_err(|_| AdapterStoreError::Corrupt)?;
        let object = value.as_object_mut().ok_or(AdapterStoreError::Corrupt)?;
        object.remove("messageId");
        object.remove("sentAt");
        if let Some(outcome) = object.get_mut("outcome").and_then(Value::as_object_mut) {
            outcome.remove("finishedAt");
        }
        Ok(value)
    }
    Ok(normalized(first)? == normalized(second)?)
}

/// Compares one regenerated outbound frame with the durable predecessor while
/// ignoring fields allocated for the current transport attempt.  The exact
/// stored frame is returned by [`ExecutionOutbox::retain_in_transaction`], so
/// this comparison only authorizes replay; it never mutates the durable bytes.
fn same_replay_facts(first: &[u8], second: &[u8]) -> Result<bool, AdapterStoreError> {
    fn normalized(bytes: &[u8]) -> Result<Value, AdapterStoreError> {
        let mut value: Value =
            serde_json::from_slice(bytes).map_err(|_| AdapterStoreError::Corrupt)?;
        let object = value.as_object_mut().ok_or(AdapterStoreError::Corrupt)?;
        for field in ["messageId", "sentAt", "boundAt", "observedAt"] {
            object.remove(field);
        }
        Ok(value)
    }
    Ok(normalized(first)? == normalized(second)?)
}

fn decode_delivery(
    delivery_id: String,
    frame: &[u8],
) -> Result<DurableExecutionDelivery, AdapterStoreError> {
    let message = serde_json::from_slice(frame).map_err(|_| AdapterStoreError::Corrupt)?;
    Ok(DurableExecutionDelivery {
        delivery_id,
        message,
    })
}

fn record_rejected_in_transaction(
    transaction: &Transaction<'_>,
    delivery_id: &str,
) -> Result<(), AdapterStoreError> {
    let bytes: Vec<u8> = transaction
        .query_row(
            "SELECT frame_json FROM execution_outbox WHERE delivery_id=?1",
            [delivery_id],
            |row| row.get(0),
        )
        .map_err(|_| AdapterStoreError::Conflict)?;
    transaction.execute(
        "INSERT INTO execution_delivery_disposition(delivery_id,original_digest,original_frame,rejected) VALUES (?1,?2,?3,1) ON CONFLICT(delivery_id) DO UPDATE SET rejected=1",
        params![delivery_id,digest(&bytes),bytes],
    ).map_err(|_| AdapterStoreError::Unavailable)?;
    transaction
        .execute(
            "UPDATE execution_outbox SET state=?2,acknowledgement_required=0 WHERE delivery_id=?1",
            params![delivery_id, SENT_ATTEMPT],
        )
        .map_err(|_| AdapterStoreError::Unavailable)?;
    Ok(())
}

struct RuntimeRow {
    delivery_id: String,
    event: RuntimeEventMessage,
}

fn runtime_rows(transaction: &Transaction<'_>) -> Result<Vec<RuntimeRow>, AdapterStoreError> {
    let mut statement = transaction
        .prepare(
            "SELECT delivery_id, frame_json FROM execution_outbox
             WHERE family = ?1 ORDER BY position",
        )
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let rows = statement
        .query_map(params![Family::Runtime.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|_| AdapterStoreError::Unavailable)?;
    rows.map(|row| {
        let (delivery_id, frame) = row.map_err(|_| AdapterStoreError::Unavailable)?;
        let message: ExecutionPortMessage =
            serde_json::from_slice(&frame).map_err(|_| AdapterStoreError::Corrupt)?;
        let ExecutionPortMessage::RuntimeEventMessage(event) = message else {
            return Err(AdapterStoreError::Corrupt);
        };
        Ok(RuntimeRow { delivery_id, event })
    })
    .collect()
}

fn event_sequence(message: &RuntimeEventMessage) -> Result<u64, AdapterStoreError> {
    u64::try_from(message.event.sequence.0).map_err(|_| AdapterStoreError::Corrupt)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use winwincode_domain::{ExecutionAckSequence, ExecutionMessageId, ExecutionSequence, Instant};
    use winwincode_execution_port::{
        generated::{
            ExecutionPortError, ExecutionPortErrorCode, ExecutionPortMessage, LeaseWriteStatus,
        },
        runtime_replay::{RuntimeReplayAckReceipt, RuntimeReplayBatch},
    };

    use super::ExecutionOutbox;
    use crate::store::{AdapterStore, AdapterStoreError};

    fn test_root(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "winwincode-codex-outbox-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    fn fixture(kind: &str) -> ExecutionPortMessage {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .expect("decode execution port fixtures");
        let message = fixture["messages"]
            .as_array()
            .expect("fixture messages")
            .iter()
            .find(|message| message["kind"] == kind)
            .expect("fixture kind")
            .clone();
        serde_json::from_value(message).expect("decode generated message")
    }

    fn runtime_pair() -> (
        winwincode_execution_port::generated::RuntimeEventMessage,
        winwincode_execution_port::generated::RuntimeEventMessage,
    ) {
        let ExecutionPortMessage::RuntimeEventMessage(first) = fixture("runtime.event") else {
            panic!("runtime fixture kind")
        };
        let mut second = first.clone();
        second.message_id = ExecutionMessageId("xmsg_00000000000000000000000029".to_owned());
        second.event.event_id =
            winwincode_domain::ExecutionEventId("xevt_0000000000000000000000002B".to_owned());
        second.event.sequence = ExecutionSequence(2);
        (first, second)
    }

    fn terminal_model_ack() -> ExecutionPortMessage {
        let ExecutionPortMessage::ModelAckMessage(mut acknowledgement) = fixture("model.ack")
        else {
            panic!("model ack fixture")
        };
        acknowledgement.ack_sequence = ExecutionAckSequence(0);
        acknowledgement.error = Some(ExecutionPortError {
            code: ExecutionPortErrorCode::Cancelled,
            message: "model exchange cancelled by Worker".to_owned(),
            retryable: false,
        });
        acknowledgement.replay_from_sequence = None;
        acknowledgement.status = LeaseWriteStatus::RejectedConflict;
        ExecutionPortMessage::ModelAckMessage(acknowledgement)
    }

    #[test]
    fn retain_before_send_loss_and_restart_replays_exact_original() {
        let root = test_root("retain-send-loss");
        let message = fixture("model.open");
        let original = serde_json::to_vec(&message).expect("serialize original");
        let delivery_id;
        {
            let store = AdapterStore::open(&root).expect("open store");
            let outbox = ExecutionOutbox::open(store).expect("open outbox");
            let delivery = outbox.retain(&message).expect("retain before send");
            delivery_id = delivery.delivery_id.clone();
            assert_eq!(outbox.pending().expect("pending"), vec![delivery]);
        }
        {
            let store = AdapterStore::open(&root).expect("restart store");
            let outbox = ExecutionOutbox::open(store).expect("restart outbox");
            let replay = outbox.pending().expect("restart pending");
            assert_eq!(replay.len(), 1);
            assert_eq!(replay[0].delivery_id, delivery_id);
            assert_eq!(
                serde_json::to_vec(&replay[0].message).expect("serialize replay"),
                original
            );
        }
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn every_response_bearing_family_retries_send_and_response_loss_exactly() {
        for (request_kind, response_kind) in [
            ("model.open", "model.chunk"),
            ("action.enforcement_request", "action.enforcement_receipt"),
            ("approval.request", "approval.decision"),
            ("input.request", "input.response"),
            ("worker.register", "worker.registration_result"),
            ("worker.heartbeat", "worker.heartbeat_ack"),
        ] {
            let root = test_root(request_kind);
            let request = fixture(request_kind);
            let original_bytes = serde_json::to_vec(&request).expect("serialize request");
            let retained;
            {
                let store = AdapterStore::open(&root).expect("open store");
                let outbox = ExecutionOutbox::open(store).expect("open outbox");
                retained = outbox.retain(&request).expect("retain request");
                assert_eq!(
                    outbox.pending().expect("send-loss retry"),
                    vec![retained.clone()]
                );
                outbox
                    .record_sent(&retained.delivery_id)
                    .expect("record attempt");
                assert_eq!(
                    outbox.pending().expect("same-process response-loss retry"),
                    vec![retained.clone()]
                );
            }

            let outbox = ExecutionOutbox::open(AdapterStore::open(&root).expect("restart store"))
                .expect("restart outbox");
            let restart_retry = outbox.pending().expect("restart response-loss retry");
            assert_eq!(restart_retry.len(), 1);
            assert_eq!(restart_retry[0].delivery_id, retained.delivery_id);
            assert_eq!(
                serde_json::to_vec(&restart_retry[0].message).expect("serialize retry"),
                original_bytes
            );

            if response_kind == "model.chunk" {
                let mut changed = fixture(response_kind);
                let ExecutionPortMessage::ModelChunkMessage(chunk) = &mut changed else {
                    panic!("model chunk fixture")
                };
                chunk.worker_session_id.0.push('X');
                assert_eq!(
                    outbox.acknowledge_response(&changed),
                    Err(AdapterStoreError::Conflict)
                );
            }
            outbox
                .acknowledge_response(&fixture(response_kind))
                .expect("exact response");
            assert!(outbox.pending().expect("compacted pending").is_empty());
            if matches!(
                response_kind,
                "input.response"
                    | "job.outcome_ack"
                    | "model.chunk"
                    | "action.enforcement_receipt"
                    | "approval.decision"
            ) {
                // The Kernel/input ledger, durable model-call frame ledger,
                // action gate or approval-operation ledger has already applied
                // this exact response. Replaying the Control Plane frame after
                // the Worker lost its ACK must remain an idempotent no-op even
                // though the durable request row is compacted.
                outbox
                    .acknowledge_response(&fixture(response_kind))
                    .expect("exact terminal response replay after compaction");
                assert!(outbox.pending().expect("replayed input pending").is_empty());
            }
            drop(outbox);
            std::fs::remove_dir_all(root).expect("remove fixture");
        }
    }

    #[test]
    fn compacted_response_replay_is_independent_of_other_pending_requests() {
        for (request_kind, response_kind) in [
            ("input.request", "input.response"),
            ("approval.request", "approval.decision"),
            ("action.enforcement_request", "action.enforcement_receipt"),
            ("model.open", "model.chunk"),
            ("job.outcome", "job.outcome_ack"),
        ] {
            let root = test_root("response-replay-with-new-request");
            let outbox = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
            outbox.retain(&fixture(request_kind)).unwrap();
            let response = fixture(response_kind);
            outbox.acknowledge_response(&response).unwrap();
            let mut unrelated = serde_json::to_value(fixture(request_kind)).unwrap();
            unrelated["messageId"] = serde_json::json!(format!("xmsg_{}", "Z".repeat(26)));
            unrelated["lease"]["jobId"] = serde_json::json!(format!("job_{}", "Z".repeat(26)));
            let unrelated = serde_json::from_value(unrelated).unwrap();
            let pending = outbox.retain(&unrelated).unwrap();
            drop(outbox);
            let outbox = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
            outbox
                .acknowledge_response(&response)
                .unwrap_or_else(|error| {
                    panic!("old {response_kind} must not conflict with another request: {error:?}")
                });
            assert_eq!(outbox.pending().unwrap(), vec![pending]);
            let mut foreign = serde_json::to_value(response).unwrap();
            foreign["lease"]["jobId"] = serde_json::json!(format!("job_{}", "Y".repeat(26)));
            assert_eq!(
                outbox.acknowledge_response(&serde_json::from_value(foreign).unwrap()),
                Err(AdapterStoreError::Conflict)
            );
            drop(outbox);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn heartbeat_gap_is_consumed_without_acknowledging_or_losing_the_request() {
        let root = test_root("heartbeat-gap-replay");
        let outbox = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
        let first = outbox.retain(&fixture("worker.heartbeat")).unwrap();
        let ExecutionPortMessage::WorkerHeartbeatMessage(mut second) = fixture("worker.heartbeat")
        else {
            unreachable!()
        };
        second.heartbeat_sequence.0 = 2;
        second.message_id = ExecutionMessageId(format!("xmsg_{}", "B".repeat(26)));
        let second = outbox
            .retain(&ExecutionPortMessage::WorkerHeartbeatMessage(second))
            .unwrap();
        let mut gap = serde_json::to_value(fixture("worker.heartbeat_ack")).unwrap();
        gap["heartbeatSequence"] = serde_json::json!(2);
        gap["status"] = serde_json::json!("rejected_worker_instance");
        gap["error"] = serde_json::json!({"code":"SEQUENCE_GAP","message":"retry the original sequence","retryable":true});
        let gap: ExecutionPortMessage = serde_json::from_value(gap).unwrap();
        outbox.acknowledge_response(&gap).unwrap();
        assert_eq!(
            outbox.pending().unwrap(),
            vec![first.clone(), second.clone()]
        );
        drop(outbox);
        let outbox = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
        outbox.acknowledge_response(&gap).unwrap();
        for invalid in 0..3 {
            let mut value = serde_json::to_value(&gap).unwrap();
            match invalid {
                0 => value["error"]["retryable"] = serde_json::json!(false),
                1 => {
                    value["workerInstanceId"] =
                        serde_json::json!(format!("wki_{}", "C".repeat(26)));
                }
                _ => value["heartbeatSequence"] = serde_json::json!(3),
            }
            assert_eq!(
                outbox.acknowledge_response(&serde_json::from_value(value).unwrap()),
                Err(AdapterStoreError::Conflict)
            );
        }
        outbox
            .acknowledge_response(&fixture("worker.heartbeat_ack"))
            .unwrap();
        let mut accepted = serde_json::to_value(fixture("worker.heartbeat_ack")).unwrap();
        accepted["heartbeatSequence"] = serde_json::json!(2);
        outbox
            .acknowledge_response(&serde_json::from_value(accepted.clone()).unwrap())
            .unwrap();
        accepted["status"] = serde_json::json!("duplicate");
        outbox
            .acknowledge_response(&serde_json::from_value(accepted).unwrap())
            .unwrap();
        outbox.acknowledge_response(&gap).unwrap();
        assert!(
            outbox.pending().unwrap().is_empty(),
            "late Gap must not resurrect acknowledged requests"
        );
        drop(outbox);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_receipt_replay_normalizes_clock_and_duplicate_status_only() {
        for (request_kind, response_kind) in [
            ("worker.register", "worker.registration_result"),
            ("worker.heartbeat", "worker.heartbeat_ack"),
        ] {
            let root = test_root("registry-replay-clock");
            let outbox = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
            outbox.retain(&fixture(request_kind)).unwrap();
            let response = fixture(response_kind);
            outbox.acknowledge_response(&response).unwrap();
            let mut replay = serde_json::to_value(response).unwrap();
            replay["status"] = serde_json::json!("duplicate");
            replay["sentAt"] = serde_json::json!("2038-01-01T00:00:02.000Z");
            replay["serverTime"] = replay["sentAt"].clone();
            let normalized = serde_json::from_value(replay.clone()).unwrap();
            outbox.acknowledge_response(&normalized).unwrap();
            replay["workerInstanceId"] = serde_json::json!(format!("wki_{}", "Z".repeat(26)));
            assert_eq!(
                outbox.acknowledge_response(&serde_json::from_value(replay).unwrap()),
                Err(AdapterStoreError::Conflict)
            );
            drop(outbox);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn refused_optional_report_has_one_durable_bounded_projection_for_every_replay_path() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        use sha2::Digest as _;
        let root = test_root("bounded-optional-projection");
        let store = AdapterStore::open(&root).unwrap();
        let outbox = ExecutionOutbox::open(store).unwrap();
        let ExecutionPortMessage::RuntimeEventMessage(mut event) = fixture("runtime.event") else {
            unreachable!()
        };
        event.event.category = winwincode_execution_port::generated::ExecutionEventCategory::Usage;
        let raw =
            serde_json::to_vec(&serde_json::json!({"report": "x".repeat(300 * 1024)})).unwrap();
        event.event.payload = Some(serde_json::from_value(serde_json::json!({
            "contentType":"application/json",
            "dataBase64":STANDARD.encode(&raw),"payloadDigest":format!("sha256:{:x}",sha2::Sha256::digest(&raw))
        })).unwrap());
        let original = ExecutionPortMessage::RuntimeEventMessage(event.clone());
        let retained = outbox.retain(&original).unwrap();
        let projected = outbox
            .degrade_auxiliary(&retained.delivery_id)
            .unwrap()
            .unwrap();
        let ExecutionPortMessage::RuntimeEventMessage(compact) = &projected.message else {
            unreachable!()
        };
        assert_eq!(compact.event.sequence, event.event.sequence);
        assert_eq!(compact.event.event_id, event.event.event_id);
        assert!(compact.event.payload.is_none());
        assert_eq!(compact.lease, event.lease);
        assert!(
            winwincode_execution_port::transport::TypedFrame::new(
                winwincode_execution_port::transport::FrameDirection::WorkerToControlPlane,
                projected.message.clone()
            )
            .is_ok()
        );
        assert_eq!(outbox.retain(&original).unwrap(), projected);
        assert_eq!(outbox.pending().unwrap(), vec![projected.clone()]);
        drop(outbox);
        let outbox = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
        assert_eq!(
            outbox.requeue_runtime_events(&[event]).unwrap(),
            vec![projected.clone()]
        );
        assert_eq!(outbox.retain(&original).unwrap(), projected);
        assert!(
            outbox
                .degrade_auxiliary(&retained.delivery_id)
                .unwrap()
                .is_none()
        );
        outbox.record_rejected(&retained.delivery_id).unwrap();
        assert!(outbox.pending().unwrap().is_empty());
        let ExecutionPortMessage::RuntimeEventMessage(event) = &original else {
            unreachable!()
        };
        assert!(outbox.has_rejected_job(&event.lease).unwrap());
        let mut foreign = event.lease.clone();
        foreign.worker_instance_id.0.push('X');
        assert!(!outbox.has_rejected_job(&foreign).unwrap());
        drop(outbox);
        let outbox = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
        assert!(outbox.is_rejected(&retained.delivery_id).unwrap());
        assert!(outbox.pending().unwrap().is_empty());
        let ExecutionPortMessage::RuntimeEventMessage(original) = original else {
            unreachable!()
        };
        assert!(
            outbox
                .requeue_runtime_events(std::slice::from_ref(&original))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            outbox.requeue(&retained.delivery_id),
            Err(AdapterStoreError::Conflict)
        );
        outbox
            .store
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM execution_outbox WHERE delivery_id=?1",
                [&retained.delivery_id],
            )
            .unwrap();
        assert!(
            outbox.has_rejected_job(&original.lease).unwrap(),
            "cleanup cannot erase task rejection proof"
        );
        drop(outbox);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejected_terminal_is_scoped_to_the_owning_worker_instance_after_restart() {
        let root = test_root("rejected-terminal-scope");
        let outbox = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
        let message = fixture("job.outcome");
        let retained = outbox.retain(&message).unwrap();
        outbox.record_rejected(&retained.delivery_id).unwrap();
        drop(outbox);
        let outbox = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
        let ExecutionPortMessage::JobOutcomeMessage(outcome) = message else {
            unreachable!()
        };
        assert!(
            outbox
                .has_rejected_terminal(&outcome.lease.worker_id, &outcome.lease.worker_instance_id)
                .unwrap()
        );
        let mut successor = outcome.lease.worker_instance_id.clone();
        successor.0.push('X');
        assert!(
            !outbox
                .has_rejected_terminal(&outcome.lease.worker_id, &successor)
                .unwrap()
        );
        assert!(outbox.pending().unwrap().is_empty());
        drop(outbox);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn heartbeat_highwater_survives_ack_compaction_and_process_restart() {
        let root = test_root("heartbeat-highwater");
        {
            let outbox = ExecutionOutbox::open(AdapterStore::open(&root).expect("open store"))
                .expect("open outbox");
            let first = fixture("worker.heartbeat");
            let ExecutionPortMessage::WorkerHeartbeatMessage(first_heartbeat) = &first else {
                panic!("heartbeat fixture")
            };
            let worker_id = first_heartbeat.worker_id.clone();
            let worker_instance_id = first_heartbeat.worker_instance_id.clone();
            outbox.retain(&first).expect("retain first heartbeat");
            assert_eq!(
                outbox.heartbeat_sequence_highwater(&worker_id, &worker_instance_id),
                Ok(1)
            );
            outbox
                .acknowledge_response(&fixture("worker.heartbeat_ack"))
                .expect("lost heartbeat acknowledgement replay");
            let ExecutionPortMessage::WorkerHeartbeatAckMessage(mut foreign) =
                fixture("worker.heartbeat_ack")
            else {
                panic!("heartbeat acknowledgement fixture")
            };
            foreign.worker_instance_id.0.push('X');
            assert_eq!(
                outbox.acknowledge_response(&ExecutionPortMessage::WorkerHeartbeatAckMessage(
                    foreign
                )),
                Err(AdapterStoreError::Conflict)
            );
            outbox
                .acknowledge_response(&fixture("worker.heartbeat_ack"))
                .expect("ack first heartbeat");
            assert!(outbox.pending().expect("compacted heartbeat").is_empty());
            assert_eq!(
                outbox.heartbeat_sequence_highwater(&worker_id, &worker_instance_id),
                Ok(1)
            );
        }
        {
            let outbox = ExecutionOutbox::open(AdapterStore::open(&root).expect("restart store"))
                .expect("restart outbox");
            let ExecutionPortMessage::WorkerHeartbeatMessage(first) = fixture("worker.heartbeat")
            else {
                panic!("heartbeat fixture")
            };
            assert_eq!(
                outbox.heartbeat_sequence_highwater(&first.worker_id, &first.worker_instance_id),
                Ok(1)
            );
            let ExecutionPortMessage::WorkerHeartbeatMessage(mut second) =
                fixture("worker.heartbeat")
            else {
                panic!("heartbeat fixture")
            };
            second.heartbeat_sequence = ExecutionSequence(2);
            second.message_id = ExecutionMessageId("xmsg_0000000000000000000000002H".to_owned());
            outbox
                .retain(&ExecutionPortMessage::WorkerHeartbeatMessage(second))
                .expect("retain second heartbeat");
            outbox
                .acknowledge_response(&fixture("worker.heartbeat_ack"))
                .expect("stale acknowledgement does not disturb next heartbeat");
            assert_eq!(outbox.pending().expect("second heartbeat pending").len(), 1);
            assert_eq!(
                outbox.heartbeat_sequence_highwater(&first.worker_id, &first.worker_instance_id),
                Ok(2)
            );
        }
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn terminal_model_ack_compacts_only_the_exact_open_same_process_and_after_restart() {
        let root = test_root("terminal-model-ack");
        let request = fixture("model.open");
        let outbox = ExecutionOutbox::open(AdapterStore::open(&root).expect("open store"))
            .expect("open outbox");
        let retained = outbox.retain(&request).expect("retain model open");
        outbox
            .record_sent(&retained.delivery_id)
            .expect("record model open attempt");
        outbox
            .acknowledge_response(&terminal_model_ack())
            .expect("accept exact terminal model acknowledgement");
        assert!(outbox.pending().expect("same-process pending").is_empty());
        drop(outbox);

        let restarted = ExecutionOutbox::open(AdapterStore::open(&root).expect("restart store"))
            .expect("restart outbox");
        assert!(restarted.pending().expect("restart pending").is_empty());
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn model_cancellation_after_first_response_preserves_both_replay_proofs() {
        let root = test_root("model-response-then-cancel");
        let outbox = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
        outbox.retain(&fixture("model.open")).unwrap();
        let first = fixture("model.chunk");
        outbox.acknowledge_response(&first).unwrap();
        let cancelled = terminal_model_ack();
        outbox.acknowledge_response(&cancelled).unwrap();
        drop(outbox);
        let outbox = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
        outbox.acknowledge_response(&first).unwrap();
        outbox.acknowledge_response(&cancelled).unwrap();
        assert!(outbox.pending().unwrap().is_empty());
        let mut foreign = serde_json::to_value(cancelled).unwrap();
        foreign["lease"]["fencingToken"] = serde_json::json!("999");
        assert_eq!(
            outbox.acknowledge_response(&serde_json::from_value(foreign).unwrap()),
            Err(AdapterStoreError::Conflict)
        );
        drop(outbox);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nonterminal_model_ack_is_transport_replay_until_send_then_compacts() {
        let root = test_root("nonterminal-model-ack");
        let acknowledgement = fixture("model.ack");
        let outbox = ExecutionOutbox::open(AdapterStore::open(&root).expect("open store"))
            .expect("open outbox");
        let retained = outbox
            .retain(&acknowledgement)
            .expect("retain model progress acknowledgement");
        assert_eq!(
            outbox.pending().expect("pending before send"),
            vec![retained.clone()]
        );
        outbox
            .record_sent(&retained.delivery_id)
            .expect("record model progress acknowledgement send");
        assert!(
            outbox
                .pending()
                .expect("sent model progress acknowledgement")
                .is_empty()
        );
        drop(outbox);
        let restarted = ExecutionOutbox::open(AdapterStore::open(&root).expect("restart store"))
            .expect("restart outbox");
        assert!(
            restarted
                .pending()
                .expect("restart model progress acknowledgement")
                .is_empty()
        );
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn all_model_roles_share_durable_cancellation_before_dispatch() {
        for wording in [
            "model exchange cancelled by Worker",
            "Observer model exchange cancelled by Worker",
        ] {
            let root = test_root("shared-model-cancellation");
            let store = AdapterStore::open(&root).unwrap();
            let outbox = ExecutionOutbox::open(store.clone()).unwrap();
            let request = fixture("model.open");
            let ExecutionPortMessage::ModelOpenMessage(open) = &request else {
                panic!("open fixture")
            };
            outbox.retain(&request).unwrap();
            assert!(ExecutionOutbox::model_start_request_allowed(&store, open).unwrap());
            let mut cancelled = terminal_model_ack();
            let ExecutionPortMessage::ModelAckMessage(ack) = &mut cancelled else {
                panic!("ack fixture")
            };
            ack.error.as_mut().unwrap().message = wording.into();
            outbox.retain(&cancelled).unwrap();
            outbox.acknowledge_response(&cancelled).unwrap();
            assert!(
                !ExecutionOutbox::model_start_request_allowed(&store, open).unwrap(),
                "reliable cancellation forbids first start before network dispatch"
            );
            assert!(
                outbox
                    .pending()
                    .unwrap()
                    .iter()
                    .all(|delivery| delivery.message != request)
            );
            let restarted = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
            assert!(!ExecutionOutbox::model_start_request_allowed(&restarted.store, open).unwrap());
            if let ExecutionPortMessage::ModelAckMessage(ack) = &mut cancelled {
                ack.error.as_mut().unwrap().message = "model exchange cancelled by Worker".into();
            }
            restarted.acknowledge_response(&cancelled).unwrap();
            assert!(!ExecutionOutbox::model_start_request_allowed(&restarted.store, open).unwrap());
            drop(restarted);
            drop(outbox);
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn changed_terminal_model_ack_keeps_the_exact_open_pending() {
        for (name, changed) in [
            ("exchange", 0_u8),
            ("lease", 1_u8),
            ("session", 2_u8),
            ("terminal-digest", 3_u8),
        ] {
            let root = test_root(name);
            let request = fixture("model.open");
            let outbox = ExecutionOutbox::open(AdapterStore::open(&root).expect("open store"))
                .expect("open outbox");
            let retained = outbox.retain(&request).expect("retain model open");
            outbox
                .record_sent(&retained.delivery_id)
                .expect("record model open attempt");
            let mut acknowledgement = terminal_model_ack();
            let ExecutionPortMessage::ModelAckMessage(message) = &mut acknowledgement else {
                panic!("terminal model acknowledgement")
            };
            match changed {
                0 => message.model_exchange_id.0.push('X'),
                1 => message.lease.lease_id.0.push('X'),
                2 => message.session_identity.product_session_id.0.push('X'),
                3 => message
                    .error
                    .as_mut()
                    .expect("terminal error")
                    .message
                    .push('X'),
                _ => unreachable!("closed changed acknowledgement cases"),
            }
            assert_eq!(
                outbox.acknowledge_response(&acknowledgement),
                Err(AdapterStoreError::Conflict)
            );
            assert_eq!(
                outbox.pending().expect("changed response pending"),
                vec![retained]
            );
            drop(outbox);
            let restarted =
                ExecutionOutbox::open(AdapterStore::open(&root).expect("restart store"))
                    .expect("restart outbox");
            assert_eq!(restarted.pending().expect("restart pending").len(), 1);
            std::fs::remove_dir_all(root).expect("remove fixture");
        }
    }

    #[test]
    fn accepted_runtime_ack_compacts_only_contiguous_cursor_without_a_gap() {
        let root = test_root("runtime-accepted");
        let outbox =
            ExecutionOutbox::open(AdapterStore::open(&root).expect("open store")).expect("outbox");
        let (first, second) = runtime_pair();
        outbox
            .retain(&ExecutionPortMessage::RuntimeEventMessage(first))
            .expect("retain first");
        outbox
            .retain(&ExecutionPortMessage::RuntimeEventMessage(second))
            .expect("retain second");
        let ExecutionPortMessage::RuntimeAckMessage(mut acknowledgement) = fixture("runtime.ack")
        else {
            panic!("runtime ack fixture")
        };
        let receipt = RuntimeReplayAckReceipt {
            status: LeaseWriteStatus::Accepted,
            ack_sequence: ExecutionAckSequence(1),
            highest_sequence: ExecutionAckSequence(2),
            replay_from_sequence: None,
            replay: None,
        };
        outbox
            .apply_runtime_ack(&acknowledgement, &receipt)
            .expect("accept ack one");
        let pending = outbox.pending().expect("pending after ack one");
        assert_eq!(pending.len(), 1);
        let ExecutionPortMessage::RuntimeEventMessage(event) = &pending[0].message else {
            panic!("remaining runtime event")
        };
        assert_eq!(event.event.sequence.0, 2);
        drop(outbox);
        let outbox =
            ExecutionOutbox::open(AdapterStore::open(&root).expect("restart store after ack one"))
                .expect("restart outbox after ack one");
        let restart_pending = outbox.pending().expect("restart cursor pending");
        assert_eq!(restart_pending.len(), 1);
        let ExecutionPortMessage::RuntimeEventMessage(event) = &restart_pending[0].message else {
            panic!("restart remaining runtime event")
        };
        assert_eq!(event.event.sequence.0, 2);

        acknowledgement.ack_sequence = ExecutionAckSequence(2);
        let receipt = RuntimeReplayAckReceipt {
            ack_sequence: ExecutionAckSequence(2),
            ..receipt
        };
        outbox
            .apply_runtime_ack(&acknowledgement, &receipt)
            .expect("accept ack two");
        assert!(outbox.pending().expect("fully compacted").is_empty());
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn rejected_runtime_ack_archives_only_its_stream_and_survives_restart() {
        for status in [
            LeaseWriteStatus::RejectedConflict,
            LeaseWriteStatus::RejectedExpiredLease,
            LeaseWriteStatus::RejectedStaleFencingToken,
            LeaseWriteStatus::RejectedWorkerInstance,
        ] {
            let root = test_root("runtime-rejected-stream");
            let outbox = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
            let (first, second) = runtime_pair();
            let lease = first.lease.clone();
            let mut other = first.clone();
            other.lease.job_id =
                winwincode_domain::ExecutionJobId("job_00000000000000000000000999".into());
            other.message_id = ExecutionMessageId("xmsg_00000000000000000000000999".into());
            let other_lease = other.lease.clone();
            for event in [first, second, other] {
                outbox
                    .retain(&ExecutionPortMessage::RuntimeEventMessage(event))
                    .unwrap();
            }
            let ExecutionPortMessage::RuntimeAckMessage(mut ack) = fixture("runtime.ack") else {
                panic!("ACK");
            };
            ack.status = status.clone();
            ack.ack_sequence = ExecutionAckSequence(0);
            let receipt = RuntimeReplayAckReceipt {
                status,
                ack_sequence: ExecutionAckSequence(0),
                highest_sequence: ExecutionAckSequence(2),
                replay_from_sequence: None,
                replay: None,
            };
            outbox.apply_runtime_ack(&ack, &receipt).unwrap();
            assert!(outbox.has_rejected_job(&lease).unwrap());
            assert!(!outbox.has_rejected_job(&other_lease).unwrap());
            assert_eq!(outbox.pending().unwrap().len(), 1);
            drop(outbox);
            let reopened = ExecutionOutbox::open(AdapterStore::open(&root).unwrap()).unwrap();
            reopened.apply_runtime_ack(&ack, &receipt).unwrap();
            assert_eq!(reopened.pending().unwrap().len(), 1);
            ack.ack_sequence = ExecutionAckSequence(1);
            assert!(
                reopened.apply_runtime_ack(&ack, &receipt).is_err(),
                "changed replay cannot overwrite rejection proof"
            );
            drop(reopened);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn gap_ack_requeues_original_runtime_bytes_in_order() {
        let root = test_root("runtime-gap");
        let outbox =
            ExecutionOutbox::open(AdapterStore::open(&root).expect("open store")).expect("outbox");
        let (first, second) = runtime_pair();
        let first_delivery = outbox
            .retain(&ExecutionPortMessage::RuntimeEventMessage(first.clone()))
            .expect("retain first");
        let second_delivery = outbox
            .retain(&ExecutionPortMessage::RuntimeEventMessage(second.clone()))
            .expect("retain second");
        outbox
            .record_sent(&first_delivery.delivery_id)
            .expect("send first");
        outbox
            .record_sent(&second_delivery.delivery_id)
            .expect("send second");
        let ExecutionPortMessage::RuntimeAckMessage(mut acknowledgement) = fixture("runtime.ack")
        else {
            panic!("runtime ack fixture")
        };
        acknowledgement.status = LeaseWriteStatus::Gap;
        acknowledgement.ack_sequence = ExecutionAckSequence(0);
        acknowledgement.replay_from_sequence = Some(ExecutionSequence(1));
        let receipt = RuntimeReplayAckReceipt {
            status: LeaseWriteStatus::Gap,
            ack_sequence: ExecutionAckSequence(0),
            highest_sequence: ExecutionAckSequence(2),
            replay_from_sequence: Some(ExecutionSequence(1)),
            replay: Some(RuntimeReplayBatch {
                ack_sequence: ExecutionAckSequence(0),
                highest_sequence: ExecutionAckSequence(2),
                events: vec![first, second],
            }),
        };
        let replay = outbox
            .apply_runtime_ack(&acknowledgement, &receipt)
            .expect("apply gap");
        assert_eq!(
            replay
                .iter()
                .map(|delivery| delivery.delivery_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                first_delivery.delivery_id.as_str(),
                second_delivery.delivery_id.as_str()
            ]
        );
        assert_eq!(outbox.pending().expect("gap pending"), replay);
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn terminal_send_loss_replays_first_outcome_same_process_and_restart() {
        let root = test_root("terminal-replay");
        let original = fixture("job.outcome");
        let original_bytes = serde_json::to_vec(&original).expect("serialize outcome");
        let delivery_id;
        {
            let outbox = ExecutionOutbox::open(AdapterStore::open(&root).expect("open store"))
                .expect("outbox");
            let delivery = outbox.retain(&original).expect("retain outcome");
            delivery_id = delivery.delivery_id.clone();
            assert_eq!(
                outbox.pending().expect("same process replay"),
                vec![delivery]
            );
            outbox.record_sent(&delivery_id).expect("send attempt");
            let retry = outbox.pending().expect("response-loss retry");
            assert_eq!(retry.len(), 1);
            assert_eq!(retry[0].delivery_id, delivery_id);
        }
        {
            let outbox = ExecutionOutbox::open(AdapterStore::open(&root).expect("restart store"))
                .expect("restart outbox");
            let replay = outbox.pending().expect("restart replay");
            assert_eq!(replay.len(), 1);
            assert_eq!(replay[0].delivery_id, delivery_id);
            assert_eq!(
                serde_json::to_vec(&replay[0].message).expect("serialize restart outcome"),
                original_bytes
            );

            let mut reconstructed = original.clone();
            {
                let ExecutionPortMessage::JobOutcomeMessage(message) = &mut reconstructed else {
                    panic!("outcome fixture")
                };
                message.message_id =
                    ExecutionMessageId("xmsg_0000000000000000000000002R".to_owned());
                message.sent_at = Instant("2026-08-24T12:00:01.000Z".to_owned());
                message.outcome.finished_at = Instant("2026-08-24T12:00:01.000Z".to_owned());
            }
            let exact = outbox
                .retain(&reconstructed)
                .expect("semantic terminal retry");
            assert_eq!(exact.delivery_id, delivery_id);
            assert_eq!(
                serde_json::to_vec(&exact.message).expect("serialize original retry"),
                original_bytes
            );

            let ExecutionPortMessage::JobOutcomeMessage(message) = &mut reconstructed else {
                panic!("outcome fixture")
            };
            message.outcome.summary.push_str(" changed");
            assert_eq!(
                outbox.retain(&reconstructed),
                Err(AdapterStoreError::Conflict)
            );

            let mut changed_ack = fixture("job.outcome_ack");
            let ExecutionPortMessage::JobOutcomeAckMessage(ack) = &mut changed_ack else {
                panic!("outcome ack fixture")
            };
            ack.worker_session_id.0.push('X');
            assert_eq!(
                outbox.acknowledge_response(&changed_ack),
                Err(AdapterStoreError::Conflict)
            );
            let mut rejected_ack = fixture("job.outcome_ack");
            let ExecutionPortMessage::JobOutcomeAckMessage(ack) = &mut rejected_ack else {
                panic!("outcome ack fixture")
            };
            ack.status =
                winwincode_execution_port::generated::JobOutcomeAckMessageStatus::RejectedConflict;
            assert_eq!(
                outbox.acknowledge_response(&rejected_ack),
                Err(AdapterStoreError::Conflict)
            );
            outbox
                .acknowledge_response(&fixture("job.outcome_ack"))
                .expect("exact terminal receipt");
            assert!(outbox.pending().expect("terminal compacted").is_empty());
            outbox
                .acknowledge_response(&fixture("job.outcome_ack"))
                .expect("lost terminal response replays after compaction");
            assert_eq!(
                outbox.acknowledge_response(&rejected_ack),
                Err(AdapterStoreError::Conflict)
            );
        }
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one scenario verifies upgrade, atomic rollback, ACK compaction and restart"
    )]
    fn terminal_snapshot_survives_ack_upgrade_and_atomic_rollback() {
        for legacy in [false, true] {
            let root = test_root("terminal-snapshot");
            let store = AdapterStore::open(&root).unwrap();

            let mut original = fixture("job.outcome");
            let ExecutionPortMessage::JobOutcomeMessage(outcome) = &mut original else {
                unreachable!()
            };
            outcome.outcome.usage = Some(
                winwincode_execution_port::generated::ExecutionOutcomeUsage::unknown(1000, 48),
            );
            outcome.outcome.last_event_sequence.0 = 12;
            let initial = if legacy {
                serde_json::json!({"terminalMessageId": outcome.message_id})
            } else {
                serde_json::json!("active")
            };
            store.save_run("run", &initial).unwrap();
            if legacy {
                ExecutionOutbox::open(store.clone())
                    .unwrap()
                    .retain(&original)
                    .unwrap();
                store
                    .lock()
                    .unwrap()
                    .execute_batch("DROP TABLE execution_terminal_outcome")
                    .unwrap();
            }
            drop(store);
            let store = AdapterStore::open(&root).unwrap();
            let ExecutionPortMessage::JobOutcomeMessage(outcome) = &original else {
                unreachable!()
            };
            let rolled_back: Result<(), AdapterStoreError> = store.transaction(|transaction| {
                AdapterStore::save_run_in_transaction(transaction, "run", &"finalized")?;
                ExecutionOutbox::retain_terminal_in_transaction(transaction, "run", outcome)?;
                Err(AdapterStoreError::Unavailable)
            });
            assert_eq!(rolled_back, Err(AdapterStoreError::Unavailable));
            assert_eq!(
                store.load_run::<serde_json::Value>("run").unwrap(),
                Some(initial)
            );
            let count: i64 = store
                .lock()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM execution_terminal_outcome",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                count,
                i64::from(legacy),
                "upgrade snapshots before ACK consumption"
            );
            let retained = store
                .transaction(|transaction| {
                    AdapterStore::save_run_in_transaction(transaction, "run", &"finalized")?;
                    ExecutionOutbox::retain_terminal_in_transaction(transaction, "run", outcome)
                })
                .unwrap();
            assert_eq!(retained.message, original);
            let outbox = ExecutionOutbox::open(store.clone()).unwrap();
            outbox
                .acknowledge_response(&fixture("job.outcome_ack"))
                .unwrap();
            assert!(outbox.pending().unwrap().is_empty());
            drop(outbox);
            drop(store);
            let store = AdapterStore::open(&root).unwrap();
            let mut reconstructed = outcome.clone();
            reconstructed.outcome.last_event_sequence.0 = 0;
            let usage = reconstructed.outcome.usage.as_mut().unwrap();
            usage.tokens = Some(54);
            usage.known_tokens = 54;
            usage.accounting_status =
                winwincode_execution_port::generated::ExecutionOutcomeUsageAccountingStatus::Known;
            usage.cost_microunits = Some(100);
            let replay = store
                .transaction(|transaction| {
                    ExecutionOutbox::retain_terminal_in_transaction(
                        transaction,
                        "run",
                        &reconstructed,
                    )
                })
                .unwrap();
            assert_eq!(replay.message, original);
            reconstructed.worker_session_id.0.push('X');
            assert_eq!(
                store.transaction(
                    |transaction| ExecutionOutbox::retain_terminal_in_transaction(
                        transaction,
                        "run",
                        &reconstructed
                    )
                ),
                Err(AdapterStoreError::Conflict)
            );
            store
                .lock()
                .unwrap()
                .execute_batch("UPDATE execution_terminal_outcome SET frame_digest = 'corrupt'")
                .unwrap();
            assert_eq!(
                store.transaction(
                    |transaction| ExecutionOutbox::retain_terminal_in_transaction(
                        transaction,
                        "run",
                        outcome
                    )
                ),
                Err(AdapterStoreError::Corrupt)
            );
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn terminal_run_finalize_and_first_outcome_retention_commit_or_rollback_together() {
        let root = test_root("terminal-transaction");
        let store = AdapterStore::open(&root).expect("open store");
        store.save_run("run", &"active").expect("save active run");
        let original = fixture("job.outcome");
        let mut changed = original.clone();
        let ExecutionPortMessage::JobOutcomeMessage(message) = &mut changed else {
            panic!("outcome fixture")
        };
        message.outcome.summary.push_str(" changed");
        let outbox = ExecutionOutbox::open(store.clone()).expect("open outbox");
        outbox.retain(&original).expect("retain first original");

        let failed = store.transaction(|transaction| {
            AdapterStore::save_run_in_transaction(transaction, "run", &"finalized")?;
            ExecutionOutbox::retain_in_transaction(transaction, &changed)
        });
        assert_eq!(failed, Err(AdapterStoreError::Conflict));
        assert_eq!(
            store
                .load_run::<String>("run")
                .expect("load rolled back run")
                .as_deref(),
            Some("active")
        );
        assert_eq!(outbox.pending().expect("original remains").len(), 1);

        let committed = store
            .transaction(|transaction| {
                AdapterStore::save_run_in_transaction(transaction, "run", &"finalized")?;
                ExecutionOutbox::retain_in_transaction(transaction, &original)
            })
            .expect("commit terminal boundary");
        assert_eq!(
            store
                .load_run::<String>("run")
                .expect("load finalized run")
                .as_deref(),
            Some("finalized")
        );
        assert_eq!(committed.message, original);
        std::fs::remove_dir_all(root).expect("remove fixture");
    }
}
