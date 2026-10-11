// SPDX-License-Identifier: Apache-2.0

use winwincode_codex::{CodexCoreAdapter, WorkerExecutionPort};
use winwincode_domain::{ExecutionMessageId, Instant};
use winwincode_execution_port::generated::ExecutionPortMessage;

use crate::{WorkerError, WorkerErrorCode, WorkerMain};

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
        let drive_error = self.poll_codex(clock()?).await.err();
        self.drain_controls(source, &mut clock).await?;
        Ok(drive_error)
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
