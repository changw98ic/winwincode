// SPDX-License-Identifier: Apache-2.0

//! Production Worker assembly for Control Plane-enforced tool actions.
//!
//! The embedded Codex adapter owns this wrapper rather than the bare Action
//! Gateway. It verifies the Control Plane signature and exact action authority,
//! then claims the invocation in the Worker's durable receipt directory before
//! the existing gateway can call the tool executor.

use std::path::Path;

use winwincode_domain::{ExecutionMessageId, Instant};
use winwincode_execution_port::{
    action_enforcement::{
        ActionEnforcementError, ActionEnforcementSigningKey, ActionReceiptUseError,
        FileActionReceiptUseStore, prepare_action_enforcement_request,
    },
    action_gateway::{
        ActionGatewayResult, CodexToolExecutor, DeterministicActionGate, PreActionDecisionRecorder,
        WorkerActionGateway, WorkerActionRequest,
    },
    generated::{ActionEnforcementReceiptMessage, ActionEnforcementRequestMessage},
};

/// Durable receipt-verifying assembly around the sole Worker Action Gateway.
pub struct DurableWorkerActionEnforcement<Policy, Gate, Recorder, Executor> {
    gateway: WorkerActionGateway<Policy, Gate, Recorder, Executor>,
    verifier: winwincode_execution_port::action_enforcement::ActionEnforcementVerifier,
    receipt_store: FileActionReceiptUseStore,
}

impl<Policy, Gate, Recorder, Executor>
    DurableWorkerActionEnforcement<Policy, Gate, Recorder, Executor>
where
    Gate: DeterministicActionGate<Policy>,
    Recorder: PreActionDecisionRecorder<Policy>,
    Executor: CodexToolExecutor,
{
    /// Installs the Control Plane verification key and opens the durable Worker
    /// receipt directory before Codex Core can execute an action.
    ///
    /// # Errors
    ///
    /// Returns a durable store error when the Worker data root is unavailable.
    pub fn open(
        worker_data_root: impl AsRef<Path>,
        signing_key: ActionEnforcementSigningKey,
        gateway: WorkerActionGateway<Policy, Gate, Recorder, Executor>,
    ) -> Result<Self, ActionReceiptUseError> {
        Ok(Self {
            gateway,
            verifier: signing_key.into_verifier(),
            receipt_store: FileActionReceiptUseStore::open(worker_data_root)?,
        })
    }

    /// Builds the exact Worker-to-Control-Plane request for a pending action.
    ///
    /// # Errors
    ///
    /// Rejects invalid or intent-mismatched actions before a frame is emitted.
    pub fn prepare_request(
        &self,
        message_id: ExecutionMessageId,
        sent_at: Instant,
        action: &WorkerActionRequest,
    ) -> Result<ActionEnforcementRequestMessage, ActionEnforcementError> {
        prepare_action_enforcement_request(message_id, sent_at, action)
    }

    /// Verifies, durably claims, locally gates, and executes one action.
    ///
    /// # Errors
    ///
    /// Returns before execution for a forged, denied, stale, cross-tenant,
    /// changed-action, already-consumed, or unavailable receipt.
    pub fn execute(
        &mut self,
        now: &Instant,
        action: &WorkerActionRequest,
        receipt: &ActionEnforcementReceiptMessage,
    ) -> ActionGatewayResult<Executor::Output, Recorder::Error, Executor::Error> {
        self.gateway.execute(
            now,
            action,
            receipt,
            &self.verifier,
            &mut self.receipt_store,
        )
    }
}
