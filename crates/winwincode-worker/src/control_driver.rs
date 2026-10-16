// SPDX-License-Identifier: Apache-2.0

use winwincode_codex::{CodexCoreAdapter, WorkerExecutionPort};
use winwincode_domain::{ExecutionMessageId, Instant};
use winwincode_execution_port::generated::ExecutionPortMessage;

use crate::{
    WorkerError, WorkerErrorCode, WorkerLifecycleState, WorkerMain, codex_model_error,
    execution_port_error, worker_error,
};

impl<Codex> WorkerMain<crate::remote_transport::RemoteWorkerPort, Codex>
where
    Codex: CodexCoreAdapter + Send + 'static,
{
    pub async fn close_expired_terminal_outbox(
        &mut self,
        now: Instant,
    ) -> Result<bool, WorkerError> {
        if self.lifecycle != WorkerLifecycleState::Stopped || !self.active.is_empty() {
            return Err(worker_error(
                WorkerErrorCode::InvalidLifecycle,
                "expired terminal cleanup requires stopped execution",
            ));
        }
        let deliveries = self
            .codex
            .pending_execution_deliveries()
            .map_err(|_| codex_model_error())?;
        let mut complete = true;
        for delivery in deliveries {
            let ExecutionPortMessage::JobOutcomeMessage(outcome) = &delivery.message else {
                continue;
            };
            if !self
                .dispatches
                .get(&outcome.lease.job_id.0)
                .is_some_and(|record| {
                    record.terminal
                        && record.run_key.attempt == outcome.lease.attempt
                        && record.run_key.fencing_token == outcome.lease.fencing_token
                        && record.worker_session_id == outcome.worker_session_id
                        && record.codex_thread_id == outcome.session_identity.codex_thread_id
                })
            {
                complete = false;
                continue;
            }
            let result = async {
                let Some(acknowledgement) = self
                    .port
                    .close_retained_terminal(&delivery)
                    .await
                    .map_err(|_| execution_port_error())?
                else {
                    return Ok(false);
                };
                if matches!(
                    acknowledgement.status,
                    winwincode_execution_port::generated::JobOutcomeAckMessageStatus::Accepted
                        | winwincode_execution_port::generated::JobOutcomeAckMessageStatus::Duplicate
                ) {
                    self.codex
                        .record_execution_delivery_sent(&delivery.delivery_id)
                        .map_err(|_| codex_model_error())?;
                    Box::pin(self.apply_control(
                        &ExecutionPortMessage::JobOutcomeAckMessage(acknowledgement),
                        now.clone(),
                    ))
                    .await?;
                    Ok(true)
                } else {
                    self.codex
                        .record_execution_delivery_rejected(&delivery.delivery_id)
                        .map_err(|_| codex_model_error())
                }
            }
            .await;
            complete &= result == Ok(true);
        }
        Ok(complete)
    }
}

pub trait WorkerControlSource {
    type Error: std::error::Error + 'static;

    fn next_control(
        &self,
    ) -> Result<Option<(ExecutionMessageId, ExecutionPortMessage)>, Self::Error>;

    fn confirm(&self, id: ExecutionMessageId) -> Result<(), Self::Error>;

    fn retry(&self, id: &ExecutionMessageId) -> Result<(), Self::Error>;
}

impl WorkerControlSource for crate::remote_transport::RemoteWorkerTransportHandle {
    type Error = crate::remote_transport::RemoteWorkerPortError;

    fn next_control(
        &self,
    ) -> Result<Option<(ExecutionMessageId, ExecutionPortMessage)>, Self::Error> {
        Self::next_control(self)
    }

    fn confirm(&self, id: ExecutionMessageId) -> Result<(), Self::Error> {
        Self::confirm(self, id)
    }

    fn retry(&self, id: &ExecutionMessageId) -> Result<(), Self::Error> {
        Self::retry(self, id)
    }
}

impl<Port, Codex> WorkerMain<Port, Codex>
where
    Port: WorkerExecutionPort,
    Codex: CodexCoreAdapter + Send + 'static,
{
    pub async fn drive_with_controls<Source, Clock>(
        &mut self,
        source: &Source,
        mut clock: Clock,
    ) -> Result<Option<WorkerError>, Box<dyn std::error::Error>>
    where
        Source: WorkerControlSource,
        Clock: FnMut() -> Result<Instant, Box<dyn std::error::Error>>,
    {
        self.drain_controls(source, &mut clock).await?;
        if self.transport_failure().is_some() {
            return Ok(None);
        }
        let heartbeat_error = self.service_heartbeat(&mut clock).await?;
        let response_controls = self.port.has_pending_controls();
        self.drain_controls(source, &mut clock).await?;
        if response_controls {
            return Ok(heartbeat_error);
        }
        let drive_error = self.poll_codex(clock()?).await.err();
        self.drain_controls(source, &mut clock).await?;
        let final_heartbeat_error = self.service_heartbeat(&mut clock).await?;
        self.drain_controls(source, &mut clock).await?;
        Ok(drive_error.or(heartbeat_error).or(final_heartbeat_error))
    }

    async fn service_heartbeat<Clock>(
        &mut self,
        clock: &mut Clock,
    ) -> Result<Option<WorkerError>, Box<dyn std::error::Error>>
    where
        Clock: FnMut() -> Result<Instant, Box<dyn std::error::Error>>,
    {
        if self.heartbeat_interval_ms.is_some()
            && self
                .next_heartbeat_due
                .is_none_or(|due| std::time::Instant::now() >= due)
        {
            #[cfg(feature = "test-support")]
            crate::mechanism_timing::record("heartbeat_entry", &serde_json::json!({}));
            let result = self.heartbeat(clock()?).await;
            #[cfg(feature = "test-support")]
            crate::mechanism_timing::record("heartbeat_exit", &serde_json::json!({}));
            return Ok(result.err());
        }
        Ok(None)
    }

    pub async fn drain_controls<Source, Clock>(
        &mut self,
        source: &Source,
        mut clock: Clock,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        Source: WorkerControlSource,
        Clock: FnMut() -> Result<Instant, Box<dyn std::error::Error>>,
    {
        let started = std::time::Instant::now();
        for _ in 0..64 {
            if started.elapsed() >= std::time::Duration::from_millis(25) {
                break;
            }
            let Some((id, message)) = source.next_control()? else {
                break;
            };
            match self.accept_control(&message, clock()?).await {
                Ok(()) => source.confirm(id)?,
                Err(error) => {
                    source.retry(&id)?;
                    eprintln!(
                        "component=worker stage=control_consume code={:?}",
                        error.code
                    );
                    if matches!(
                        error.code,
                        WorkerErrorCode::ExecutionPort
                            | WorkerErrorCode::ExecutionBackpressure
                            | WorkerErrorCode::CodexDurableState
                    ) || self.has_pending_durable_evidence()
                    {
                        return Ok(());
                    }
                    return Err(Box::new(error));
                }
            }
        }
        Ok(())
    }
}
