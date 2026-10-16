// SPDX-License-Identifier: Apache-2.0

use super::{
    ActiveJobLifecycle, CodexCoreAdapter, DurableExecutionDelivery, ExecutionAckSequence,
    ExecutionEventCategory, ExecutionOutcomeStatus, ExecutionPortErrorCode,
    ExecutionPortFailureKind, ExecutionPortMessage, WorkerError, WorkerErrorCode,
    WorkerExecutionPort, WorkerMain, active_delivery_job_id, candidate_delivery_job_id,
    codex_model_error, device_model, execution_port_error, port_error,
    retained_lease_matches_current, worker_error,
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

    async fn dispatch_execution_effect(
        &mut self,
        message: ExecutionPortMessage,
    ) -> Result<EffectDispatchOutcome, WorkerError> {
        if self.device_models.is_none()
            && let ExecutionPortMessage::ModelOpenMessage(open) = &message
            && self.local_model_start_deadline(open)?.is_none()
        {
            return Err(worker_error(
                WorkerErrorCode::ExecutionMessageRejected,
                "new model request has no current execution authority",
            ));
        }
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
        // Quiescing a failed transport is not a user cancellation. Preserve
        // the actual lifecycle so an existing user cancel still owns terminal
        // status, while a delivery failure remains infrastructure failure.
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

    fn recovered_interaction_delivery(&self, delivery: &DurableExecutionDelivery) -> bool {
        matches!(
            &delivery.message,
            ExecutionPortMessage::ApprovalRequestMessage(_)
                | ExecutionPortMessage::InputRequestMessage(_)
                | ExecutionPortMessage::ActionEnforcementRequestMessage(_)
        ) && active_delivery_job_id(&delivery.message)
            .is_some_and(|job_id| self.deferred_core_interaction_jobs.contains(job_id))
    }

    fn durable_delivery_allowed(
        &mut self,
        delivery: &DurableExecutionDelivery,
    ) -> Result<bool, WorkerError> {
        if matches!(&delivery.message, ExecutionPortMessage::ModelOpenMessage(_))
            && self.device_models.is_none()
            && !self
                .requested_model_deliveries
                .contains(&delivery.delivery_id)
        {
            return Ok(false);
        }
        if self.recovered_interaction_delivery(delivery)
            && self
                .recovery_sent_delivery_ids
                .contains(&delivery.delivery_id)
        {
            return Ok(false);
        }
        if let Some(job_id) = active_delivery_job_id(&delivery.message)
            && !self.active.contains_key(job_id)
        {
            match &delivery.message {
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

    fn pending_outbox_retries(&mut self) -> Result<Vec<DurableExecutionDelivery>, WorkerError> {
        let mut retries = Vec::new();
        for id in self.outbox_retry_ids.clone() {
            if let Some(delivery) = self
                .codex
                .pending_execution_delivery(&id)
                .map_err(|_| codex_model_error())?
            {
                retries.push(delivery);
            } else {
                self.outbox_retry_ids.remove(&id);
                self.outbox_flush_cursor = None;
            }
        }
        Ok(retries)
    }

    pub(super) async fn flush_durable_execution_deliveries(&mut self) -> Result<(), WorkerError> {
        let deadline = tokio::time::Instant::now() + MAX_OUTBOX_BATCH_DURATION;
        let mut retries = self.pending_outbox_retries()?;
        let mut blocked = retries.clone();
        let mut visited = std::collections::HashSet::new();
        let mut sent_bytes = 0;
        let mut deferred = None;
        'batches: for retry in [true, false] {
            let deliveries = if retry {
                std::mem::take(&mut retries)
            } else {
                let batch = self
                    .codex
                    .pending_execution_delivery_batch(
                        self.outbox_flush_cursor.as_deref(),
                        MAX_OUTBOX_BATCH_FRAMES,
                    )
                    .map_err(|_| codex_model_error())?;
                if batch.is_empty() {
                    self.outbox_flush_cursor = None;
                }
                batch
            };
            for delivery in deliveries {
                if self.port.has_pending_controls() || tokio::time::Instant::now() >= deadline {
                    break 'batches;
                }
                let previous_cursor = self.outbox_flush_cursor.clone();
                if !retry {
                    self.outbox_flush_cursor = Some(delivery.delivery_id.clone());
                }
                if !visited.insert(delivery.delivery_id.clone()) {
                    continue;
                }
                if self.sent_delivery_ids.contains(&delivery.delivery_id) {
                    continue;
                }
                if blocked.iter().any(|prior| {
                    prior.delivery_id != delivery.delivery_id
                        && execution_delivery_depends_on(&delivery.message, &prior.message)
                }) {
                    continue;
                }
                if !self.durable_delivery_allowed(&delivery)? {
                    continue;
                }
                let recovered_interaction = self.recovered_interaction_delivery(&delivery);
                let frame_bytes = serde_json::to_vec(&delivery.message)
                    .map_err(|_| codex_model_error())?
                    .len();
                if sent_bytes > 0 && sent_bytes + frame_bytes > MAX_OUTBOX_BATCH_BYTES {
                    self.outbox_flush_cursor = previous_cursor;
                    break 'batches;
                }
                let delivery_id = delivery.delivery_id.clone();
                let sent = self.dispatch_retained_effect(delivery.clone()).await;
                if let Err(error) = sent {
                    if error.code == WorkerErrorCode::ModelStartDeferred {
                        deferred.get_or_insert(error);
                        continue;
                    }
                    if error.code == WorkerErrorCode::ExecutionPort {
                        if self.outbox_retry_ids.insert(delivery_id) {
                            blocked.push(delivery);
                        }
                        deferred = Some(error);
                        continue;
                    }
                    if error.code != WorkerErrorCode::ExecutionMessageRejected {
                        self.outbox_flush_cursor = previous_cursor;
                    }
                    return Err(error);
                }
                sent_bytes += frame_bytes;
                if self.outbox_retry_ids.remove(&delivery_id) {
                    self.outbox_flush_cursor = None;
                    blocked.retain(|prior| prior.delivery_id != delivery_id);
                }
                if recovered_interaction {
                    self.recovery_sent_delivery_ids.insert(delivery_id);
                }
                if let ExecutionPortMessage::RuntimeEventMessage(event) = delivery.message
                    && let Some(active) = self.active.get_mut(&event.lease.job_id.0)
                    && event.event.sequence.0 > active.last_event_sequence.0
                {
                    active.last_event_sequence = ExecutionAckSequence(event.event.sequence.0);
                }
                if self.port.has_pending_controls() {
                    break 'batches;
                }
            }
        }
        deferred.map_or(Ok(()), Err)
    }
}

fn execution_delivery_depends_on(
    message: &ExecutionPortMessage,
    prior: &ExecutionPortMessage,
) -> bool {
    use ExecutionPortMessage::{
        ArtifactChunkMessage, ArtifactOpenMessage, JobCancelAckMessage, JobDispatchResultMessage,
        JobOutcomeMessage, ModelAckMessage, ModelChunkMessage, RuntimeEventMessage,
        SessionBindingMessage,
    };
    if matches!(message, JobCancelAckMessage(_) | ModelAckMessage(_)) {
        return false;
    }
    match (prior, message) {
        (RuntimeEventMessage(first), RuntimeEventMessage(next)) => {
            winwincode_execution_port::runtime_replay::runtime_event_stream_key(first)
                == winwincode_execution_port::runtime_replay::runtime_event_stream_key(next)
                && first.event.sequence.0 < next.event.sequence.0
        }
        (RuntimeEventMessage(event), JobOutcomeMessage(outcome)) => {
            retained_lease_matches_current(&event.lease, &outcome.lease)
                && event.session_identity == outcome.session_identity
                && event.event.sequence.0 <= outcome.outcome.last_event_sequence.0
        }
        (ArtifactOpenMessage(open), ArtifactChunkMessage(chunk)) => {
            open.artifact.artifact_id == chunk.artifact_id
                && retained_lease_matches_current(&open.lease, &chunk.lease)
                && open.session_identity == chunk.session_identity
        }
        (ArtifactChunkMessage(first), ArtifactChunkMessage(next)) => {
            first.artifact_id == next.artifact_id
                && retained_lease_matches_current(&first.lease, &next.lease)
                && first.session_identity == next.session_identity
                && first.sequence.0 < next.sequence.0
        }
        (ArtifactOpenMessage(open), JobOutcomeMessage(outcome)) => {
            retained_lease_matches_current(&open.lease, &outcome.lease)
                && open.session_identity == outcome.session_identity
                && outcome
                    .outcome
                    .artifacts
                    .iter()
                    .any(|artifact| artifact.artifact_id == open.artifact.artifact_id)
        }
        (ArtifactChunkMessage(chunk), JobOutcomeMessage(outcome)) => {
            retained_lease_matches_current(&chunk.lease, &outcome.lease)
                && chunk.session_identity == outcome.session_identity
                && outcome
                    .outcome
                    .artifacts
                    .iter()
                    .any(|artifact| artifact.artifact_id == chunk.artifact_id)
        }
        (ModelChunkMessage(first), ModelChunkMessage(next)) => {
            first.model_exchange_id == next.model_exchange_id
                && retained_lease_matches_current(&first.lease, &next.lease)
                && first.session_identity == next.session_identity
                && first.sequence.0 < next.sequence.0
        }
        (ModelChunkMessage(chunk), JobOutcomeMessage(outcome)) => {
            retained_lease_matches_current(&chunk.lease, &outcome.lease)
                && chunk.session_identity == outcome.session_identity
        }
        (JobDispatchResultMessage(_) | SessionBindingMessage(_), _) => {
            let prior_lease = match prior {
                JobDispatchResultMessage(result) => &result.lease,
                SessionBindingMessage(binding) => &binding.lease,
                _ => unreachable!(),
            };
            let lease = match message {
                RuntimeEventMessage(event) => &event.lease,
                JobOutcomeMessage(outcome) => &outcome.lease,
                ArtifactOpenMessage(open) => &open.lease,
                ArtifactChunkMessage(chunk) => &chunk.lease,
                ModelChunkMessage(chunk) => &chunk.lease,
                SessionBindingMessage(binding) => &binding.lease,
                JobDispatchResultMessage(result) => &result.lease,
                ExecutionPortMessage::ModelOpenMessage(open) => &open.lease,
                ExecutionPortMessage::ApprovalRequestMessage(request) => &request.lease,
                ExecutionPortMessage::InputRequestMessage(request) => &request.lease,
                ExecutionPortMessage::ActionEnforcementRequestMessage(request) => &request.lease,
                _ => return false,
            };
            retained_lease_matches_current(prior_lease, lease)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_dependencies_only_block_the_same_execution() {
        let fixtures: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let message = |kind: &str| -> ExecutionPortMessage {
            serde_json::from_value(
                fixtures["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["kind"] == kind)
                    .unwrap()
                    .clone(),
            )
            .unwrap()
        };
        for kind in ["job.dispatch_result", "session.binding"] {
            let prior = message(kind);
            let outcome = message("job.outcome");
            assert!(execution_delivery_depends_on(&outcome, &prior));
            for changed_identity in ["attempt", "instance"] {
                let mut changed = outcome.clone();
                let ExecutionPortMessage::JobOutcomeMessage(terminal) = &mut changed else {
                    unreachable!()
                };
                if changed_identity == "attempt" {
                    terminal.lease.attempt += 1;
                } else {
                    terminal.lease.worker_instance_id.0 = "wki_00000000000000000000000999".into();
                }
                assert!(!execution_delivery_depends_on(&changed, &prior));
            }
            let mut renewed = outcome;
            let ExecutionPortMessage::JobOutcomeMessage(terminal) = &mut renewed else {
                unreachable!()
            };
            terminal.lease.expires_at.0 = "2026-08-24T12:15:00.000Z".into();
            assert!(execution_delivery_depends_on(&renewed, &prior));
        }
    }
}
