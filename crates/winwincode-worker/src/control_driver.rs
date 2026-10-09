// SPDX-License-Identifier: Apache-2.0

//! Shared production driver ordering for validated controls and outbound work.

use winwincode_codex::{CodexCoreAdapter, WorkerExecutionPort};
use winwincode_domain::{ExecutionMessageId, Instant};
use winwincode_execution_port::generated::ExecutionPortMessage;

use crate::{WorkerError, WorkerErrorCode, WorkerMain};

/// Exact control receipt ownership, shared by the remote transport and fixtures.
pub trait WorkerControlSource {
    type Error: std::error::Error + 'static;

    /// Reads the next validated control without confirming its receipt.
    ///
    /// # Errors
    /// Returns a transport or inbox-state failure.
    fn next_control(
        &self,
    ) -> Result<Option<(ExecutionMessageId, ExecutionPortMessage)>, Self::Error>;

    /// Confirms the receipt after its durable effects have been accepted.
    ///
    /// # Errors
    /// Returns a transport or receipt-state failure.
    fn confirm(&self, id: ExecutionMessageId) -> Result<(), Self::Error>;

    /// Releases a receipt for another consumption attempt.
    ///
    /// # Errors
    /// Returns a transport or receipt-state failure.
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
    /// Consumes controls before outbound work and immediately after its response.
    /// The clock is sampled at consumption, never copied from the start of I/O.
    ///
    /// # Errors
    /// Returns a control transport or permanent lifecycle failure. A retryable
    /// drive failure is returned separately after controls have been consumed.
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
        let drive_error = self.poll_codex(clock()?).await.err();
        self.drain_controls(source, &mut clock).await?;
        Ok(drive_error)
    }

    /// Drains validated controls and confirms only accepted durable effects.
    ///
    /// # Errors
    /// Returns permanent control rejection or unavailable transport state.
    pub async fn drain_controls<Source, Clock>(
        &mut self,
        source: &Source,
        mut clock: Clock,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        Source: WorkerControlSource,
        Clock: FnMut() -> Result<Instant, Box<dyn std::error::Error>>,
    {
        while let Some((id, message)) = source.next_control()? {
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
                        WorkerErrorCode::ExecutionPort | WorkerErrorCode::ExecutionBackpressure
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
