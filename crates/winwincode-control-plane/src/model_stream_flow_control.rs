// SPDX-License-Identifier: Apache-2.0

//! Stateless orchestration across the canonical Provider Gateway and model
//! request pool. The pool remains the only frame/capacity authority and the
//! Gateway remains the only Provider transport/settlement authority.

use std::fmt;

use winwincode_domain::{Instant, ModelExchangeId};
use winwincode_execution_port::generated::ModelAckMessage;

use crate::{
    CanonicalModelStreamFrame, ModelFrameAckReceipt, ModelFrameWriteReceipt, ModelRequestPool,
    ModelRequestPoolError, ModelRequestTerminalOutcome, ModelRequestTerminalReceipt,
    ModelStreamFrame, ModelStreamReadControl, ProviderGateway, ProviderGatewayError,
    ProviderGatewayTerminal, ProviderGatewayTerminalOutcome, ProviderGatewayTerminalProgressPort,
    ProviderGatewayTerminalReceipt, ProviderStreamControlReceipt,
};

/// Stable flow-control failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelStreamFlowErrorKind {
    InvalidBatch,
    Pool,
    Gateway,
}

/// Bounded failure from the cross-authority coordinator.
#[derive(Debug)]
pub struct ModelStreamFlowError {
    kind: ModelStreamFlowErrorKind,
    pool: Option<ModelRequestPoolError>,
    gateway: Option<ProviderGatewayError>,
}

impl ModelStreamFlowError {
    #[must_use]
    pub const fn kind(&self) -> ModelStreamFlowErrorKind {
        self.kind
    }
}

impl fmt::Display for ModelStreamFlowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            ModelStreamFlowErrorKind::InvalidBatch => "canonical model stream batch is invalid",
            ModelStreamFlowErrorKind::Pool => "model request pool transition failed",
            ModelStreamFlowErrorKind::Gateway => "Provider Gateway transition failed",
        })
    }
}

impl std::error::Error for ModelStreamFlowError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.pool
            .as_ref()
            .map(|error| error as &dyn std::error::Error)
            .or_else(|| {
                self.gateway
                    .as_ref()
                    .map(|error| error as &dyn std::error::Error)
            })
    }
}

impl From<ModelRequestPoolError> for ModelStreamFlowError {
    fn from(error: ModelRequestPoolError) -> Self {
        Self {
            kind: ModelStreamFlowErrorKind::Pool,
            pool: Some(error),
            gateway: None,
        }
    }
}

impl From<ProviderGatewayError> for ModelStreamFlowError {
    fn from(error: ProviderGatewayError) -> Self {
        Self {
            kind: ModelStreamFlowErrorKind::Gateway,
            pool: None,
            gateway: Some(error),
        }
    }
}

/// Result of applying one canonical Provider event batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelStreamFlowWriteReceipt {
    pub pool: ModelFrameWriteReceipt,
    pub provider_control: Option<ProviderStreamControlReceipt>,
    pub gateway_terminal: Option<ProviderGatewayTerminalReceipt>,
}

/// Result of a Worker acknowledgement and any resulting Provider resume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelStreamFlowAckReceipt {
    pub pool: ModelFrameAckReceipt,
    pub provider_control: Option<ProviderStreamControlReceipt>,
}

/// Exact cross-authority cancellation result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelStreamFlowCancellationReceipt {
    pub gateway: ProviderGatewayTerminalReceipt,
    pub pool: ModelRequestTerminalReceipt,
}

/// The only orchestration seam joining Provider reads, buffered frames, Worker
/// acknowledgements, and terminal slot release. It owns no exchange state.
pub struct ModelStreamFlowCoordinator<'borrow, 'storage> {
    pool: &'borrow mut ModelRequestPool,
    gateway: &'borrow mut ProviderGateway<'storage>,
}

impl<'borrow, 'storage> ModelStreamFlowCoordinator<'borrow, 'storage> {
    #[must_use]
    pub const fn new(
        pool: &'borrow mut ModelRequestPool,
        gateway: &'borrow mut ProviderGateway<'storage>,
    ) -> Self {
        Self { pool, gateway }
    }

    /// Applies one already-converted Provider event batch. The caller retains
    /// the frames and retries the exact batch after a backpressure receipt.
    ///
    /// Terminal Provider release/settlement precedes admission-slot release;
    /// exact retries replay both authorities without a second release.
    ///
    /// # Errors
    ///
    /// Rejects inconsistent terminal facts and propagates pool/Gateway errors.
    pub fn offer_provider_batch(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        frames: &[CanonicalModelStreamFrame],
        terminal: Option<ProviderGatewayTerminal>,
        observed_at: &Instant,
    ) -> Result<ModelStreamFlowWriteReceipt, ModelStreamFlowError> {
        self.offer_provider_batch_inner(model_exchange_id, frames, terminal, None, observed_at)
    }

    /// Applies a Provider batch with durable terminal-side-effect checkpoints.
    ///
    /// # Errors
    ///
    /// Rejects inconsistent terminal facts and propagates pool, Gateway, or
    /// checkpoint failures.
    pub fn offer_provider_batch_with_progress(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        frames: &[CanonicalModelStreamFrame],
        terminal: Option<ProviderGatewayTerminal>,
        progress: &dyn ProviderGatewayTerminalProgressPort,
        observed_at: &Instant,
    ) -> Result<ModelStreamFlowWriteReceipt, ModelStreamFlowError> {
        self.offer_provider_batch_inner(
            model_exchange_id,
            frames,
            terminal,
            Some((progress, observed_at)),
            observed_at,
        )
    }

    fn offer_provider_batch_inner(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        frames: &[CanonicalModelStreamFrame],
        terminal: Option<ProviderGatewayTerminal>,
        progress: Option<(&dyn ProviderGatewayTerminalProgressPort, &Instant)>,
        observed_at: &Instant,
    ) -> Result<ModelStreamFlowWriteReceipt, ModelStreamFlowError> {
        let pool_frames = pool_frames(frames, terminal)?;
        self.pool.validate_frame_batch(&pool_frames)?;
        let gateway_terminal = terminal
            .map(|command| match progress {
                Some((progress, observed_at)) => self.gateway.apply_terminal_with_progress(
                    model_exchange_id,
                    command,
                    progress,
                    observed_at,
                ),
                None => self
                    .gateway
                    .apply_terminal(model_exchange_id, command, observed_at),
            })
            .transpose()?;
        let pool = self.pool.push_frames(model_exchange_id, &pool_frames)?;
        let provider_control = if terminal.is_none() {
            self.synchronize_provider_read(model_exchange_id, pool.read_control)?
        } else {
            None
        };
        Ok(ModelStreamFlowWriteReceipt {
            pool,
            provider_control,
            gateway_terminal,
        })
    }

    /// Advances the only buffered-frame cursor and resumes Provider reading
    /// only after the route-local low watermark is reached.
    ///
    /// # Errors
    ///
    /// Propagates pool/Gateway errors while leaving both transitions replayable.
    pub fn acknowledge(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        sequence: u64,
    ) -> Result<ModelStreamFlowAckReceipt, ModelStreamFlowError> {
        let pool = self.pool.acknowledge(model_exchange_id, sequence)?;
        let provider_control =
            self.synchronize_provider_read(model_exchange_id, pool.read_control)?;
        Ok(ModelStreamFlowAckReceipt {
            pool,
            provider_control,
        })
    }

    /// Applies an authoritative Worker cancellation in Gateway→Pool order so
    /// Provider resources settle before the active admission slot is granted.
    ///
    /// # Errors
    ///
    /// Propagates exact Worker authority, Gateway, or pool failures.
    pub fn cancel_from_worker(
        &mut self,
        acknowledgement: &ModelAckMessage,
    ) -> Result<ModelStreamFlowCancellationReceipt, ModelStreamFlowError> {
        let gateway = self.gateway.cancel_from_worker(acknowledgement)?;
        let pool = self.pool.cancel(&acknowledgement.model_exchange_id)?;
        Ok(ModelStreamFlowCancellationReceipt { gateway, pool })
    }

    /// Cancels with durable checkpoints around every terminal side effect.
    ///
    /// # Errors
    ///
    /// Propagates exact Worker authority, Gateway, checkpoint, or pool failures.
    pub fn cancel_from_worker_with_progress(
        &mut self,
        acknowledgement: &ModelAckMessage,
        progress: &dyn ProviderGatewayTerminalProgressPort,
    ) -> Result<ModelStreamFlowCancellationReceipt, ModelStreamFlowError> {
        let gateway = self
            .gateway
            .cancel_from_worker_with_progress(acknowledgement, progress)?;
        let pool = self.pool.cancel(&acknowledgement.model_exchange_id)?;
        Ok(ModelStreamFlowCancellationReceipt { gateway, pool })
    }

    fn synchronize_provider_read(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        read_control: ModelStreamReadControl,
    ) -> Result<Option<ProviderStreamControlReceipt>, ModelStreamFlowError> {
        match read_control {
            ModelStreamReadControl::Read => self
                .gateway
                .set_provider_read_paused(model_exchange_id, false)
                .map(Some)
                .map_err(Into::into),
            ModelStreamReadControl::Paused => self
                .gateway
                .set_provider_read_paused(model_exchange_id, true)
                .map(Some)
                .map_err(Into::into),
            ModelStreamReadControl::Closed => Ok(None),
        }
    }
}

fn pool_frames(
    frames: &[CanonicalModelStreamFrame],
    terminal: Option<ProviderGatewayTerminal>,
) -> Result<Vec<ModelStreamFrame>, ModelStreamFlowError> {
    let terminal_outcome = terminal.map(ProviderGatewayTerminal::outcome);
    let terminal = frames
        .last()
        .is_some_and(CanonicalModelStreamFrame::is_terminal);
    if frames.is_empty()
        || terminal != terminal_outcome.is_some()
        || frames[..frames.len().saturating_sub(1)]
            .iter()
            .any(CanonicalModelStreamFrame::is_terminal)
    {
        return Err(invalid_batch());
    }
    let pool_terminal = terminal_outcome.map(|outcome| match outcome {
        ProviderGatewayTerminalOutcome::Succeeded => ModelRequestTerminalOutcome::Succeeded,
        ProviderGatewayTerminalOutcome::Failed => ModelRequestTerminalOutcome::Failed,
        ProviderGatewayTerminalOutcome::Cancelled => ModelRequestTerminalOutcome::Cancelled,
    });
    if pool_terminal == Some(ModelRequestTerminalOutcome::Cancelled) {
        return Err(invalid_batch());
    }
    Ok(frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let payload = frame.payload_json().as_bytes().to_vec();
            if index + 1 == frames.len() {
                match pool_terminal {
                    Some(outcome) => ModelStreamFrame::terminal(frame.sequence(), payload, outcome),
                    None => ModelStreamFrame::data(frame.sequence(), payload),
                }
            } else {
                ModelStreamFrame::data(frame.sequence(), payload)
            }
        })
        .collect())
}

const fn invalid_batch() -> ModelStreamFlowError {
    ModelStreamFlowError {
        kind: ModelStreamFlowErrorKind::InvalidBatch,
        pool: None,
        gateway: None,
    }
}
