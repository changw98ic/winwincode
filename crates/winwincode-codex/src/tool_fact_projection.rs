// SPDX-License-Identifier: Apache-2.0
use super::{
    CodexPoll, ProductionCodexAdapter, ProductionCodexError, canonical_id, map_store_error,
    unavailable, unknown_thread,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ExecutionEventId, ExecutionMessageId, ExecutionSequence, Instant, SchemaVersion, Sha256Digest,
};
use winwincode_execution_port::{
    generated::{
        CoreToolRuntimeFactPayload, CoreToolRuntimeFactPayloadSchemaVersion, EncodedPayload,
        ExecutionEventCategory, ExecutionEventRecord, ExecutionPortMessage, RuntimeEventMessage,
        RuntimeEventMessageKind,
    },
    replay::{ReplayAuthority, ReplayDecision, ReplayStore},
    runtime_replay::{RuntimeReplayIdentity, RuntimeReplayResponder},
};
use winwincode_kernel::KernelToolRuntimeEvent;

const CONTENT_TYPE: &str = "application/vnd.winwincode.core-tool-fact+json";

impl ProductionCodexAdapter {
    /// Request the final Core close and forward recorded receipts while it
    /// finishes. Seal the final cursor only after that same owner has closed.
    pub(super) async fn poll_final_tool_facts(
        &mut self,
        run_key: &str,
        now: &Instant,
    ) -> Result<Option<CodexPoll>, ProductionCodexError> {
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        if run.record.terminal.is_none()
            && run.record.final_candidate_freeze.is_none()
            && run.record.delegated_stop.is_none()
        {
            return Ok(None);
        }
        if let Some(cursor) = run.record.core_tool_final_cursor {
            if cursor != run.record.core_tool_cursor || run.record.core_tool_pending.is_some() {
                return Err(unavailable());
            }
            return Ok(None);
        }
        let session = run.record.kernel_session_id.clone();
        // Core can persist closed-cell facts before a shutdown hook or MCP client
        // finishes. Forward that evidence between close attempts; withholding it
        // can strand a caller that needs the fact to release its external wait.
        // Start shutdown first so a running producer cannot prolong this drain.
        if run.kernel_close_pending
            && let Some(fact) = self.poll_tool_facts(run_key, &session, now).await?
        {
            return Ok(Some(fact));
        }
        if self
            .runs
            .get(run_key)
            .ok_or_else(unknown_thread)?
            .kernel_live
        {
            match self.kernel.close_session(&session).await {
                Ok(()) => {}
                // The original terminal remains authoritative while the same
                // Core session finishes closing. A later poll can wait again.
                Err(error) if error.code() == "SESSION_SHUTDOWN_TIMEOUT" => {
                    self.runs
                        .get_mut(run_key)
                        .ok_or_else(unknown_thread)?
                        .kernel_close_pending = true;
                    return Ok(Some(CodexPoll::Pending));
                }
                Err(_) => return Err(unavailable()),
            }
            let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
            run.kernel_live = false;
            run.kernel_close_pending = false;
        }
        if let Some(fact) = self.poll_tool_facts(run_key, &session, now).await? {
            return Ok(Some(fact));
        }
        let record = &mut self
            .runs
            .get_mut(run_key)
            .ok_or_else(unknown_thread)?
            .record;
        record.core_tool_final_cursor = Some(record.core_tool_cursor);
        self.persist_run(run_key)?;
        Ok(None)
    }

    /// Drain original Core receipts before consuming a terminal Codex event.
    /// The single pending frame bridges crashes between replay, outbox and cursor commits.
    pub(super) async fn poll_tool_facts(
        &mut self,
        run_key: &str,
        session: &str,
        now: &Instant,
    ) -> Result<Option<CodexPoll>, ProductionCodexError> {
        let record = &self.runs.get(run_key).ok_or_else(unknown_thread)?.record;
        if record.core_tool_pending.is_none() {
            let events = self
                .kernel
                .tool_runtime_events(session, record.core_tool_cursor, 1)
                .await
                .map_err(|_| unavailable())?;
            let Some(event) = events.into_iter().next() else {
                return Ok(None);
            };
            let pending = self.prepare_tool_fact(run_key, session, event, now)?;
            self.runs
                .get_mut(run_key)
                .ok_or_else(unknown_thread)?
                .record
                .core_tool_pending = Some(pending);
            self.persist_run(run_key)?;
        }
        self.retain_pending_tool_fact(run_key)
    }

    pub(super) fn retain_pending_tool_fact(
        &mut self,
        run_key: &str,
    ) -> Result<Option<CodexPoll>, ProductionCodexError> {
        let Some(mut message) = self
            .runs
            .get(run_key)
            .ok_or_else(unknown_thread)?
            .record
            .core_tool_pending
            .clone()
        else {
            return Ok(None);
        };
        let binding = &self.runs.get(run_key).ok_or_else(unknown_thread)?.binding;
        let identity = RuntimeReplayIdentity {
            lease: binding.authority.lease.clone(),
            worker_session_id: binding.authority.worker_session_id.clone(),
            session_identity: binding.authority.session_identity.clone(),
            codex_thread_id: binding.canonical_thread_id.clone(),
        };
        let mut original_lease = message.lease.clone();
        let original_expiry = original_lease.expires_at.clone();
        original_lease.expires_at = identity.lease.expires_at.clone();
        if original_lease != identity.lease
            || original_expiry.0 > identity.lease.expires_at.0
            || message.worker_session_id != identity.worker_session_id
            || message.session_identity != identity.session_identity
            || message.codex_thread_id != identity.codex_thread_id
        {
            return Err(unavailable());
        }
        self.bridge
            .authority()
            .validate_active_lease(&identity.stream_key(), &identity)
            .map_err(|_| unavailable())?;
        let prior = ReplayStore::load(&mut self.store, &identity.stream_key())
            .map_err(map_store_error)?
            .unwrap_or_default();
        let sequence = u64::try_from(message.event.sequence.0).map_err(|_| unavailable())?;
        if prior.highest_sequence >= sequence {
            if prior.ack_sequence < sequence {
                let encoded = serde_json::to_vec(&message).map_err(|_| unavailable())?;
                if !prior
                    .events
                    .iter()
                    .any(|frame| frame.sequence == sequence && frame.frame == encoded)
                {
                    return Err(unavailable());
                }
            }
        } else {
            // Before first retention, a lease extension may refresh the transport
            // stamp. Once retained, the original frame remains immutable.
            if message.lease != identity.lease {
                message.lease = identity.lease.clone();
                self.runs
                    .get_mut(run_key)
                    .ok_or_else(unknown_thread)?
                    .record
                    .core_tool_pending = Some(message.clone());
                self.persist_run(run_key)?;
            }
            match RuntimeReplayResponder::default()
                .retain_runtime_event(&mut self.store, &self.bridge.authority(), &message)
                .map_err(|_| unavailable())?
            {
                ReplayDecision::Accepted { .. } | ReplayDecision::Duplicate { .. } => {}
                ReplayDecision::Gap { .. } | ReplayDecision::Conflict { .. } => {
                    return Err(unavailable());
                }
            }
        }
        let snapshot = ReplayStore::load(&mut self.store, &identity.stream_key())
            .map_err(map_store_error)?
            .ok_or_else(unavailable)?;
        let acked = snapshot.ack_sequence
            >= u64::try_from(message.event.sequence.0).map_err(|_| unavailable())?;
        if !acked {
            // Idempotent insertion also repairs a crash after replay retention but
            // before the first outbox insertion. ACKed frames remain compacted.
            self.outbox
                .retain(&ExecutionPortMessage::RuntimeEventMessage(message.clone()))
                .map_err(map_store_error)?;
        }
        let payload = message.event.payload.as_ref().ok_or_else(unavailable)?;
        let bytes = STANDARD
            .decode(&payload.data_base64)
            .map_err(|_| unavailable())?;
        let source: CoreToolRuntimeFactPayload =
            serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        let record = &mut self
            .runs
            .get_mut(run_key)
            .ok_or_else(unknown_thread)?
            .record;
        record.core_tool_cursor = source.source_sequence.0;
        record.core_tool_pending = None;
        self.persist_run(run_key)?;
        Ok(Some(if acked {
            CodexPoll::Pending
        } else {
            CodexPoll::RuntimeTrace(Box::new(message))
        }))
    }

    fn prepare_tool_fact(
        &mut self,
        run_key: &str,
        session: &str,
        event: KernelToolRuntimeEvent,
        now: &Instant,
    ) -> Result<RuntimeEventMessage, ProductionCodexError> {
        if event.source_sequence <= 0 || event.fact_json.len() > 32_768 {
            return Err(unavailable());
        }
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        let identity = RuntimeReplayIdentity {
            lease: run.binding.authority.lease.clone(),
            worker_session_id: run.binding.authority.worker_session_id.clone(),
            session_identity: run.binding.authority.session_identity.clone(),
            codex_thread_id: run.binding.canonical_thread_id.clone(),
        };
        let sequence = ReplayStore::load(&mut self.store, &identity.stream_key())
            .map_err(map_store_error)?
            .unwrap_or_default()
            .highest_sequence
            .checked_add(1)
            .ok_or_else(unavailable)?;
        let bytes = serde_json::to_vec(&CoreToolRuntimeFactPayload {
            schema_version: CoreToolRuntimeFactPayloadSchemaVersion::WinwincodeCoreToolFactV1,
            source_thread_id: session.into(),
            source_sequence: ExecutionSequence(event.source_sequence),
            fact_json: event.fact_json,
        })
        .map_err(|_| unavailable())?;
        Ok(RuntimeEventMessage {
            kind: RuntimeEventMessageKind::RuntimeEvent,
            schema_version: SchemaVersion::WinwincodeV1,
            message_id: ExecutionMessageId(canonical_id(
                "xmsg",
                b"core-tool-fact-message",
                run_key,
                sequence,
            )),
            sent_at: now.clone(),
            lease: identity.lease,
            worker_session_id: identity.worker_session_id,
            session_identity: identity.session_identity,
            codex_thread_id: identity.codex_thread_id,
            event: ExecutionEventRecord {
                event_id: ExecutionEventId(canonical_id(
                    "xevt",
                    b"core-tool-fact-event",
                    run_key,
                    sequence,
                )),
                sequence: ExecutionSequence(i64::try_from(sequence).map_err(|_| unavailable())?),
                occurred_at: now.clone(),
                category: ExecutionEventCategory::Activity,
                summary: "Core tool runtime fact".into(),
                payload: Some(EncodedPayload {
                    content_type: CONTENT_TYPE.into(),
                    data_base64: STANDARD.encode(&bytes),
                    payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
                }),
            },
        })
    }
}

#[cfg(test)]
#[path = "tool_fact_projection_tests.rs"]
mod tests;
