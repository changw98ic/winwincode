// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use winwincode_domain::{ExecutionMessageId, Instant};
use winwincode_execution_port::{
    action_enforcement::{
        ActionEnforcementError, ActionReceiptUseError, FileActionReceiptUseStore,
        prepare_action_enforcement_request,
    },
    action_gateway::{
        ActionGatewayResult, CodexToolExecutor, DeterministicActionGate, PreActionDecisionRecorder,
        WorkerActionGateway, WorkerActionRequest,
    },
    generated::{ActionEnforcementReceiptMessage, ActionEnforcementRequestMessage},
};

pub struct DurableWorkerActionEnforcement<Policy, Gate, Recorder, Executor> {
    gateway: WorkerActionGateway<Policy, Gate, Recorder, Executor>,
    receipt_store: FileActionReceiptUseStore,
}

impl<Policy, Gate, Recorder, Executor>
    DurableWorkerActionEnforcement<Policy, Gate, Recorder, Executor>
where
    Gate: DeterministicActionGate<Policy>,
    Recorder: PreActionDecisionRecorder<Policy>,
    Executor: CodexToolExecutor,
{
    pub fn open(
        worker_data_root: impl AsRef<Path>,
        gateway: WorkerActionGateway<Policy, Gate, Recorder, Executor>,
    ) -> Result<Self, ActionReceiptUseError> {
        Ok(Self {
            gateway,
            receipt_store: FileActionReceiptUseStore::open(worker_data_root)?,
        })
    }

    pub fn prepare_request(
        &self,
        message_id: ExecutionMessageId,
        sent_at: Instant,
        action: &WorkerActionRequest,
    ) -> Result<ActionEnforcementRequestMessage, ActionEnforcementError> {
        prepare_action_enforcement_request(message_id, sent_at, action)
    }

    pub fn execute(
        &mut self,
        now: &Instant,
        action: &WorkerActionRequest,
        receipt: &ActionEnforcementReceiptMessage,
    ) -> ActionGatewayResult<Executor::Output, Recorder::Error, Executor::Error> {
        self.gateway
            .execute(now, action, receipt, &mut self.receipt_store)
    }
}
