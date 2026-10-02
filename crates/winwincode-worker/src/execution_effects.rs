// SPDX-License-Identifier: Apache-2.0

//! Durable effect handoff and bounded dispatch for every Worker role.
//!
//! Producers enqueue immutable intents. Only lifecycle drivers dispatch them;
//! transport acceptance and local model handling never stand for business success.

use super::{
    ActiveJobLifecycle, CodexCoreAdapter, DurableExecutionDelivery, ExecutionAckSequence,
    ExecutionEventCategory, ExecutionOutcomeStatus, ExecutionPortErrorCode,
    ExecutionPortFailureKind, ExecutionPortMessage, WorkerError, WorkerErrorCode,
    WorkerExecutionPort, WorkerMain, active_delivery_job_id, candidate_delivery_job_id,
    codex_model_error, device_model, execution_port_error, port_error, worker_error,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectDispatchOutcome {
    RemoteFrameAccepted,
    LocalModelHandled,
}

const MAX_OUTBOX_BATCH_FRAMES: usize = 64;
const MAX_OUTBOX_BATCH_BYTES: usize = 512 * 1024;
const MAX_OUTBOX_BATCH_DURATION: std::time::Duration = std::time::Duration::from_millis(250);

impl<Port, Codex> WorkerMain<Port, Codex>
where
    Port: WorkerExecutionPort,
    Codex: CodexCoreAdapter + Send + 'static,
{
    /// Saves a mandatory effect before its originating operation can be confirmed.
    /// Never invokes a transport or Provider. Replays keep the exact original bytes.
    pub(super) fn enqueue_effect(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<(), WorkerError> {
        let delivery = self.retain_execution_message(message)?;
        if matches!(message, ExecutionPortMessage::ModelOpenMessage(_)) {
            self.requested_model_deliveries
                .insert(delivery.delivery_id.clone());
        }
        self.enqueue_retained_effect(&delivery)
    }

    pub(super) fn enqueue_retained_effect(
        &mut self,
        delivery: &DurableExecutionDelivery,
    ) -> Result<(), WorkerError> {
        self.codex
            .requeue_execution_delivery(&delivery.delivery_id)
            .map_err(|_| codex_model_error())?;
        self.sent_delivery_ids.remove(&delivery.delivery_id);
        Ok(())
    }

    /// The driver is the sole owner of outbound side effects. Local handoff and
    /// remote frame acceptance are distinct from reliable enqueue and business success.
    async fn dispatch_execution_effect(
        &mut self,
        message: ExecutionPortMessage,
    ) -> Result<EffectDispatchOutcome, WorkerError> {
        let (start_deadline, start_guard) = if self.device_models.is_some()
            && let ExecutionPortMessage::ModelOpenMessage(open) = &message
        {
            if let Some(deadline) = self.local_model_start_deadline(open)? {
                let (anchor, observed_at) = self.driver_clock.anchor();
                (
                    Some(deadline),
                    self.codex
                        .local_model_start_guard(open, anchor, observed_at)
                        .map_err(|_| codex_model_error())?,
                )
            } else {
                if !self
                    .device_models
                    .as_ref()
                    .expect("local model lane exists")
                    .start_recorded(open)
                    .map_err(|_| codex_model_error())?
                {
                    return Err(worker_error(
                        WorkerErrorCode::ExecutionMessageRejected,
                        "unstarted local model request has no current execution authority",
                    ));
                }
                // An exact started exchange restores its result, never another invocation.
                (Some(std::time::Instant::now()), None)
            }
        } else {
            (None, None)
        };
        if let Some(models) = &mut self.device_models {
            match models
                .send(&message, start_deadline, start_guard)
                .map_err(|_| codex_model_error())?
            {
                device_model::DeviceModelSendOutcome::Handled => {
                    return Ok(EffectDispatchOutcome::LocalModelHandled);
                }
                device_model::DeviceModelSendOutcome::Unhandled => {}
                device_model::DeviceModelSendOutcome::NotStarted => {
                    return Err(worker_error(
                        WorkerErrorCode::ModelStartDeferred,
                        "local model request has not started",
                    ));
                }
            }
        }
        self.port
            .send(message)
            .await
            .map_err(|error| match Port::failure_kind(&error) {
                ExecutionPortFailureKind::Backpressure => worker_error(
                    WorkerErrorCode::ExecutionBackpressure,
                    "remote Worker is backpressured",
                ),
                ExecutionPortFailureKind::Unavailable => execution_port_error(),
                ExecutionPortFailureKind::MessageRejected => worker_error(
                    WorkerErrorCode::ExecutionMessageRejected,
                    "ExecutionPort frame rejected",
                ),
                ExecutionPortFailureKind::Terminal => worker_error(
                    WorkerErrorCode::ExecutionTerminal,
                    "ExecutionPort cannot continue",
                ),
            })?;
        Ok(EffectDispatchOutcome::RemoteFrameAccepted)
    }

    fn local_model_start_deadline(
        &mut self,
        open: &winwincode_execution_port::generated::ModelOpenMessage,
    ) -> Result<Option<std::time::Instant>, WorkerError> {
        let Some(active) = self.active.get(&open.lease.job_id.0) else {
            return Ok(None);
        };
        let mut prior = open.lease.clone();
        prior.expires_at = active.lease.expires_at.clone();
        if active.lifecycle != ActiveJobLifecycle::Running
            || prior != active.lease
            || open.lease.expires_at.0 > active.lease.expires_at.0
            || open.worker_session_id != active.worker_session_id
            || !self
                .codex
                .model_start_session_allowed(open, &active.session_identity)
                .map_err(|_| codex_model_error())?
        {
            return Ok(None);
        }
        Ok(self
            .driver_clock
            .start_deadline(&active.lease.issued_at, &active.lease.expires_at))
    }

    pub(super) async fn dispatch_retained_effect(
        &mut self,
        delivery: DurableExecutionDelivery,
    ) -> Result<(), WorkerError> {
        if self
            .codex
            .execution_delivery_is_rejected(&delivery.delivery_id)
            .map_err(|_| codex_model_error())?
        {
            return Err(worker_error(
                WorkerErrorCode::ExecutionMessageRejected,
                "execution delivery already rejected",
            ));
        }
        if let Err(error) = self
            .dispatch_execution_effect(delivery.message.clone())
            .await
        {
            if error.code != WorkerErrorCode::ExecutionMessageRejected {
                return Err(error);
            }
            if let Some(projected) = self
                .codex
                .degrade_auxiliary_execution_delivery(&delivery.delivery_id)
                .map_err(|_| codex_model_error())?
            {
                match self.dispatch_execution_effect(projected.message).await {
                    Ok(_) => {}
                    Err(error) if error.code != WorkerErrorCode::ExecutionMessageRejected => {
                        return Err(error);
                    }
                    Err(_) => {
                        // Refusing the legal, small placeholder makes the runtime channel
                        // unusable. Keep it pending for recovery and preserve business facts.
                        self.delivery_failure = Some(WorkerErrorCode::ExecutionTerminal);
                        return Err(worker_error(
                            WorkerErrorCode::ExecutionTerminal,
                            "bounded runtime projection cannot be delivered",
                        ));
                    }
                }
            } else {
                if matches!(&delivery.message, ExecutionPortMessage::RuntimeEventMessage(event)
                    if event.event.category == ExecutionEventCategory::Usage)
                {
                    self.delivery_failure = Some(WorkerErrorCode::ExecutionTerminal);
                    return Err(worker_error(
                        WorkerErrorCode::ExecutionTerminal,
                        "bounded runtime projection cannot be delivered",
                    ));
                }
                Box::pin(self.finish_rejected_delivery(&delivery)).await?;
                return Err(error);
            }
        }
        self.codex
            .record_execution_delivery_sent(&delivery.delivery_id)
            .map_err(|_| codex_model_error())?;
        self.requested_model_deliveries
            .remove(&delivery.delivery_id);
        self.sent_delivery_ids.insert(delivery.delivery_id);
        Ok(())
    }

    async fn finish_rejected_delivery(
        &mut self,
        delivery: &DurableExecutionDelivery,
    ) -> Result<(), WorkerError> {
        if !self
            .codex
            .record_execution_delivery_rejected(&delivery.delivery_id)
            .map_err(|_| codex_model_error())?
        {
            return Err(worker_error(
                WorkerErrorCode::ExecutionMessageRejected,
                "adapter cannot archive rejected frame",
            ));
        }
        let Some(job_id) = active_delivery_job_id(&delivery.message).map(str::to_owned) else {
            self.delivery_failure = Some(WorkerErrorCode::ExecutionMessageRejected);
            return Ok(());
        };
        if !self.active.contains_key(&job_id) {
            self.delivery_failure = Some(WorkerErrorCode::ExecutionMessageRejected);
            return Ok(());
        }
        if matches!(delivery.message, ExecutionPortMessage::JobOutcomeMessage(_)) {
            self.delivery_failure = Some(WorkerErrorCode::ExecutionMessageRejected);
            if let Some(record) = self.dispatches.get_mut(&job_id) {
                record.terminal = true;
            }
            self.active.remove(&job_id);
            return Ok(());
        }
        Box::pin(self.finish_rejected_job(&job_id)).await
    }

    pub(super) async fn finish_rejected_job(&mut self, job_id: &str) -> Result<(), WorkerError> {
        let active = self
            .active
            .get(job_id)
            .cloned()
            .ok_or_else(codex_model_error)?;
        let now = self
            .driver_clock
            .timestamp()
            .ok_or_else(codex_model_error)?;
        Box::pin(
            self.codex
                .fail_required_execution_delivery(&active.codex_thread_id, &now),
        )
        .await
        .map_err(|_| codex_model_error())?;
        if let Some(pending) = self.pending_candidates.get(job_id).cloned() {
            if pending.artifact.is_none() {
                self.codex
                    .begin_candidate_artifact_cancel(&pending.authority)
                    .map_err(|_| codex_model_error())?;
                self.codex
                    .cancel_candidate_artifact(&pending.authority)
                    .map_err(|_| codex_model_error())?;
            }
            self.pending_candidates.remove(job_id);
        }
        // The job has stopped. Archive its obsolete evidence and interactive intents;
        // the explicit failed terminal is retained separately and remains retryable.
        let pending = self
            .codex
            .pending_execution_deliveries()
            .map_err(|_| codex_model_error())?;
        for frame in pending {
            if active_delivery_job_id(&frame.message) == Some(job_id)
                && !matches!(frame.message, ExecutionPortMessage::JobOutcomeMessage(_))
                && !matches!(&frame.message, ExecutionPortMessage::ModelAckMessage(ack) if ack.error.is_some())
            {
                self.codex
                    .record_execution_delivery_rejected(&frame.delivery_id)
                    .map_err(|_| codex_model_error())?;
            }
        }
        if let Some(active) = self.active.get_mut(job_id) {
            active.lifecycle = ActiveJobLifecycle::Cancelling;
        }
        Box::pin(self.finish_job(
            job_id,
            ExecutionOutcomeStatus::InfrastructureError,
            "required execution delivery rejected",
            Vec::new(),
            None,
            Some(port_error(
                ExecutionPortErrorCode::ExecutionFailed,
                "required execution delivery rejected",
                false,
            )),
            now,
        ))
        .await
    }

    fn durable_delivery_allowed(
        &mut self,
        delivery: &DurableExecutionDelivery,
    ) -> Result<bool, WorkerError> {
        if let Some(job_id) = active_delivery_job_id(&delivery.message)
            && !self.active.contains_key(job_id)
        {
            match &delivery.message {
                // Durable Control Plane frames must still flush after the
                // Worker drops an in-memory Job. Blocking RuntimeEvent /
                // JobOutcome / diagnostic artifact frames here strands
                // verification evidence forever and leaves Delivery
                // verdict=null even when Core already produced
                // independent-verification-result. The frame lease is the
                // authority; Server validates fencing. Candidate uploads
                // remain gated by the pending-candidate checks below.
                ExecutionPortMessage::RuntimeEventMessage(_)
                | ExecutionPortMessage::JobDispatchResultMessage(_)
                | ExecutionPortMessage::SessionBindingMessage(_)
                | ExecutionPortMessage::ModelAckMessage(_)
                | ExecutionPortMessage::ModelChunkMessage(_)
                | ExecutionPortMessage::JobOutcomeMessage(_)
                | ExecutionPortMessage::ArtifactChunkMessage(_)
                | ExecutionPortMessage::ArtifactOpenMessage(_)
                | ExecutionPortMessage::ModelOpenMessage(_) => {}
                ExecutionPortMessage::ApprovalRequestMessage(_)
                | ExecutionPortMessage::InputRequestMessage(_)
                | ExecutionPortMessage::ActionEnforcementRequestMessage(_) => {
                    return Ok(false);
                }
                _ => return Ok(false),
            }
        }
        if candidate_delivery_job_id(&delivery.message).is_some_and(|job_id| {
            !self.pending_candidates.contains_key(job_id)
                || !self
                    .active
                    .get(job_id)
                    .is_some_and(|active| active.lifecycle == ActiveJobLifecycle::Running)
        }) {
            return Ok(false);
        }
        if matches!(
            delivery.message,
            ExecutionPortMessage::ArtifactOpenMessage(_)
                | ExecutionPortMessage::ArtifactChunkMessage(_)
        ) && !self
            .codex
            .candidate_artifact_delivery_allowed(&delivery.message)
            .map_err(|_| codex_model_error())?
        {
            return Ok(false);
        }
        Ok(true)
    }

    pub(super) async fn flush_durable_execution_deliveries(&mut self) -> Result<(), WorkerError> {
        let deadline = tokio::time::Instant::now() + MAX_OUTBOX_BATCH_DURATION;
        let deliveries = self
            .codex
            .pending_execution_delivery_batch(
                self.outbox_flush_cursor.as_deref(),
                MAX_OUTBOX_BATCH_FRAMES,
            )
            .map_err(|_| codex_model_error())?;
        if deliveries.is_empty() {
            self.outbox_flush_cursor = None;
            return Ok(());
        }
        let mut sent_bytes = 0;
        for delivery in deliveries {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            let previous_cursor = self.outbox_flush_cursor.clone();
            self.outbox_flush_cursor = Some(delivery.delivery_id.clone());
            if self.sent_delivery_ids.contains(&delivery.delivery_id) {
                continue;
            }
            if matches!(&delivery.message, ExecutionPortMessage::ModelOpenMessage(_))
                && self.device_models.is_none()
                && !self
                    .requested_model_deliveries
                    .contains(&delivery.delivery_id)
            {
                // Remote paid calls remain owned by their Core recovery path. The local
                // Device store deduplicates exact identities and never re-invokes an
                // already started call; definite local startup refusals can retry here.
                continue;
            }
            let recovered_interaction = matches!(
                &delivery.message,
                ExecutionPortMessage::ApprovalRequestMessage(_)
                    | ExecutionPortMessage::InputRequestMessage(_)
                    | ExecutionPortMessage::ActionEnforcementRequestMessage(_)
            ) && active_delivery_job_id(&delivery.message)
                .is_some_and(|job_id| self.deferred_core_interaction_jobs.contains(job_id));
            if recovered_interaction
                && self
                    .recovery_sent_delivery_ids
                    .contains(&delivery.delivery_id)
            {
                continue;
            }
            if !self.durable_delivery_allowed(&delivery)? {
                continue;
            }
            let frame_bytes = serde_json::to_vec(&delivery.message)
                .map_err(|_| codex_model_error())?
                .len();
            if sent_bytes > 0 && sent_bytes + frame_bytes > MAX_OUTBOX_BATCH_BYTES {
                self.outbox_flush_cursor = previous_cursor;
                break;
            }
            let runtime_cursor = match &delivery.message {
                ExecutionPortMessage::RuntimeEventMessage(event) => {
                    Some((event.lease.job_id.0.clone(), event.event.sequence.0))
                }
                _ => None,
            };
            let delivery_id = delivery.delivery_id.clone();
            sent_bytes += frame_bytes;
            // Finish an in-flight exchange under the port's own timeout. In
            // particular, do not cancel its receipt/accounting commit halfway.
            let sent = self.dispatch_retained_effect(delivery).await;
            if let Err(error) = sent {
                if error.code != WorkerErrorCode::ExecutionMessageRejected {
                    // Keep the refused frame first on retry. Worker-wide
                    // failure yields immediately to control intake/heartbeat.
                    self.outbox_flush_cursor = previous_cursor;
                    return Err(error);
                }
                // Permanent refusal has been durably archived and the affected job
                // stopped (or its optional report projected). Resume other jobs next turn.
                return Err(error);
            }
            if recovered_interaction {
                self.recovery_sent_delivery_ids.insert(delivery_id);
            }
            if let Some((job_id, sequence)) = runtime_cursor
                && let Some(active) = self.active.get_mut(&job_id)
                && sequence > active.last_event_sequence.0
            {
                active.last_event_sequence = ExecutionAckSequence(sequence);
            }
        }
        Ok(())
    }
}
