// SPDX-License-Identifier: Apache-2.0

//! Execution-side deadlines for durable Core interactions.
//!
//! A Control Plane expiry projection does not answer a Core waiter. Keep a
//! local timeout intent before submitting the rejection, and retain the
//! original request as its deadline and recovery authority. This is not a
//! synthetic Control Plane decision or a permission grant.

use super::{
    CodexPoll, InputResponseMessageStatus, ModelLeaseAuthority, ProductionCodexAdapter,
    ProductionCodexError, ProductionCodexErrorKind, StoredApprovalOperation,
    StoredApprovalOperationKind, StoredApprovalOperationState, StoredInputOperation,
    StoredInputOperationState, StoredRun, approval_request_message, canonical_instant,
    canonical_parts_id, conflict, kernel_error, load_stored_run, map_bridge_error, map_store_error,
    private_payload_digest, retained_lease_matches_current, unavailable, unknown_thread,
};
use codex_protocol::request_user_input::{RequestUserInputAnswer, RequestUserInputResponse};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use winwincode_domain::{ExecutionMessageId, Instant, SessionIdentity};
use winwincode_execution_port::generated::{
    ApprovalDecisionMessage, ApprovalRequestMessage, ExecutionPortMessage, InputRequestMessage,
    InputResponseMessage,
};
use winwincode_kernel::{ApprovalDecision, ApprovalKind, ApprovalResponse};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct StoredInteractionTimeout {
    cause_code: String,
    pub(super) request: ExecutionPortMessage,
    request_digest: String,
    operation_request_digest: String,
    observed_at: Instant,
    applied_kernel_session_id: Option<String>,
    #[serde(default)]
    waiting_for_exact_event: bool,
}

fn request_identity(
    request: &ExecutionPortMessage,
) -> Option<(&ExecutionMessageId, &Instant, &SessionIdentity)> {
    match request {
        ExecutionPortMessage::ApprovalRequestMessage(request) => Some((
            &request.message_id,
            &request.expires_at,
            &request.session_identity,
        )),
        ExecutionPortMessage::InputRequestMessage(request) => Some((
            &request.message_id,
            &request.expires_at,
            &request.session_identity,
        )),
        _ => None,
    }
}

pub(super) fn input_response_value(
    response: &InputResponseMessage,
) -> Result<Option<String>, ProductionCodexError> {
    match response.status {
        InputResponseMessageStatus::Provided => {
            let value = response.value.as_ref().ok_or_else(conflict)?;
            if value.value.trim().is_empty() {
                return Err(conflict());
            }
            Ok(Some(value.value.clone()))
        }
        InputResponseMessageStatus::Cancelled | InputResponseMessageStatus::Expired => {
            if response.value.is_some() {
                return Err(conflict());
            }
            Ok(None)
        }
    }
}

impl ProductionCodexAdapter {
    /// Retire expired transport frames before registration can replay the
    /// outbox. Core settlement remains a separate, durable action after the
    /// exact run has been installed.
    pub(super) fn expire_retained_interactions(
        &mut self,
        now: &Instant,
    ) -> Result<(), ProductionCodexError> {
        if !canonical_instant(now) {
            return Err(conflict());
        }
        for delivery in self.outbox.pending().map_err(map_store_error)? {
            let Some((_, deadline, identity)) = request_identity(&delivery.message) else {
                continue;
            };
            if !canonical_instant(deadline) {
                return Err(conflict());
            }
            if now.0 < deadline.0 {
                continue;
            }
            let (run_key, bytes) = self
                .store
                .load_model_thread_lineage(&identity.codex_thread_id.0)
                .map_err(map_store_error)?
                .ok_or_else(conflict)?;
            let authority: ModelLeaseAuthority =
                serde_json::from_slice(&bytes).map_err(|_| conflict())?;
            let mut record = match self.runs.get(&run_key) {
                Some(run) => run.record.clone(),
                None => load_stored_run(&self.store, &run_key)?.ok_or_else(conflict)?,
            };
            if record.terminal.is_some() {
                continue;
            }
            if record.canonical_thread_id != identity.codex_thread_id
                || authority.session_identity != *identity
            {
                return Err(conflict());
            }
            self.retain_transport_timeout(
                &run_key,
                &mut record,
                &delivery.message,
                &authority,
                now,
            )?;
        }
        Ok(())
    }

    fn retain_transport_timeout(
        &mut self,
        run_key: &str,
        record: &mut StoredRun,
        request: &ExecutionPortMessage,
        authority: &ModelLeaseAuthority,
        now: &Instant,
    ) -> Result<(), ProductionCodexError> {
        let (id, _, _) = request_identity(request).ok_or_else(conflict)?;
        let lease = match request {
            ExecutionPortMessage::ApprovalRequestMessage(request) => &request.lease,
            ExecutionPortMessage::InputRequestMessage(request) => &request.lease,
            _ => return Err(conflict()),
        };
        if !retained_lease_matches_current(lease, &authority.lease) {
            return Err(conflict());
        }
        if let Some(timeout) = record.interaction_timeouts.iter().find(|timeout| {
            request_identity(&timeout.request).is_some_and(|(stored, _, _)| stored == id)
        }) {
            if timeout.request != *request {
                return Err(conflict());
            }
        } else {
            let Some(operation_request_digest) =
                self.pending_interaction_digest(run_key, request)?
            else {
                return Ok(());
            };
            let waiting_for_exact_event = !self.runs.contains_key(run_key)
                || record.recovered_interaction_requests.contains(id);
            record.interaction_timeouts.push(StoredInteractionTimeout {
                cause_code: "INTERACTION_DEADLINE_EXPIRED".to_owned(),
                request_digest: private_payload_digest(
                    b"winwincode.interaction-timeout-request.v1",
                    request,
                )?,
                request: request.clone(),
                operation_request_digest,
                observed_at: now.clone(),
                applied_kernel_session_id: None,
                waiting_for_exact_event,
            });
            record.last_activity_at = now.clone();
            self.store
                .save_run(run_key, record)
                .map_err(map_store_error)?;
            if let Some(run) = self.runs.get_mut(run_key) {
                run.record = record.clone();
            }
        }
        // Persist the timeout intent before suppressing retransmission. This
        // acknowledges only the local transport, never a Control Plane decision.
        self.finish_interaction_delivery(request)
    }

    pub(super) async fn ensure_approval_deadline(
        &mut self,
        operation: &StoredApprovalOperation,
        decision: &ApprovalDecisionMessage,
        received_at: &Instant,
    ) -> Result<(), ProductionCodexError> {
        let authority = &self
            .runs
            .get(&operation.run_key)
            .ok_or_else(unknown_thread)?
            .binding
            .authority;
        let original = self.original_approval_request(operation, authority)?;
        if received_at.0 >= original.expires_at.0 || decision.decided_at.0 >= original.expires_at.0
        {
            self.settle_interaction_deadlines(&operation.run_key, received_at)
                .await?;
            return Err(ProductionCodexError::new(
                ProductionCodexErrorKind::Authority,
                "approval deadline expired",
            ));
        }
        Ok(())
    }

    pub(super) async fn ensure_input_deadline(
        &mut self,
        operation: &StoredInputOperation,
        response: &InputResponseMessage,
        received_at: &Instant,
    ) -> Result<(), ProductionCodexError> {
        let message_id = canonical_parts_id(
            "xmsg",
            b"winwincode-kernel-input-message.v1",
            &[operation.input_request_id.as_bytes()],
        );
        let Some(ExecutionPortMessage::InputRequestMessage(original)) = self
            .outbox
            .retained_interaction(&message_id)
            .map_err(map_store_error)?
        else {
            return Err(conflict());
        };
        if received_at.0 >= original.expires_at.0
            || response.responded_at.0 >= original.expires_at.0
        {
            self.settle_interaction_deadlines(&operation.run_key, received_at)
                .await?;
            return Err(ProductionCodexError::new(
                ProductionCodexErrorKind::Authority,
                "input deadline expired",
            ));
        }
        Ok(())
    }

    pub(super) fn recover_pending_interaction_requests(
        &self,
        run_key: &str,
        record: &mut StoredRun,
    ) -> Result<(), ProductionCodexError> {
        for delivery in self.outbox.pending().map_err(map_store_error)? {
            let Some((id, _, identity)) = request_identity(&delivery.message) else {
                continue;
            };
            if identity.codex_thread_id == record.canonical_thread_id
                && self
                    .pending_interaction_digest(run_key, &delivery.message)?
                    .is_some()
                && !record.recovered_interaction_requests.contains(id)
            {
                record.recovered_interaction_requests.push(id.clone());
            }
        }
        Ok(())
    }

    pub(super) fn recover_interaction_timeouts(
        &self,
        run_key: &str,
        record: &mut StoredRun,
    ) -> Result<(), ProductionCodexError> {
        for timeout in &mut record.interaction_timeouts {
            let resolution = private_payload_digest(
                b"winwincode.interaction-deadline-expired.v1",
                &timeout.request,
            )?;
            let resolved = match &timeout.request {
                ExecutionPortMessage::ApprovalRequestMessage(request) => {
                    let operation = self
                        .store
                        .load_approval_operation(&request.approval_id.0)
                        .map_err(map_store_error)?
                        .ok_or_else(conflict)?;
                    if operation.run_key != run_key
                        || operation.request_digest != timeout.operation_request_digest
                    {
                        return Err(conflict());
                    }
                    if operation.state == StoredApprovalOperationState::Resolved
                        && operation.resolution_digest.as_deref() != Some(resolution.as_str())
                    {
                        return Err(conflict());
                    }
                    operation.state == StoredApprovalOperationState::Resolved
                }
                ExecutionPortMessage::InputRequestMessage(request) => {
                    let operation = self
                        .store
                        .load_input_operation(&request.input_request_id.0)
                        .map_err(map_store_error)?
                        .ok_or_else(conflict)?;
                    if operation.run_key != run_key
                        || operation.request_digest != timeout.operation_request_digest
                    {
                        return Err(conflict());
                    }
                    if operation.state == StoredInputOperationState::Resolved
                        && operation.resolution_digest.as_deref() != Some(resolution.as_str())
                    {
                        return Err(conflict());
                    }
                    operation.state == StoredInputOperationState::Resolved
                }
                _ => return Err(conflict()),
            };
            if resolved {
                // The previous process may have stopped between operation
                // resolution and transport cleanup. Complete that once here.
                self.finish_interaction_delivery(&timeout.request)?;
            }
            // Submission acceptance does not prove a resumed waiter consumed it.
            // Both approval and input responses wait for the exact Core request.
            timeout.waiting_for_exact_event = true;
        }
        Ok(())
    }

    pub(super) fn original_approval_request(
        &self,
        operation: &StoredApprovalOperation,
        authority: &ModelLeaseAuthority,
    ) -> Result<ApprovalRequestMessage, ProductionCodexError> {
        let generated = approval_request_message(operation, authority);
        match self
            .outbox
            .retained_interaction(&generated.message_id.0)
            .map_err(map_store_error)?
        {
            Some(ExecutionPortMessage::ApprovalRequestMessage(original))
                if original.approval_id == generated.approval_id
                    && original.session_identity == authority.session_identity
                    && original.worker_session_id == authority.worker_session_id
                    && retained_lease_matches_current(&original.lease, &authority.lease) =>
            {
                Ok(original)
            }
            None => Ok(generated),
            Some(_) => Err(conflict()),
        }
    }

    pub(super) fn has_interaction_timeout(&self, run_key: &str, id: &str) -> bool {
        self.runs.get(run_key).is_some_and(|run| {
            run.record
                .interaction_timeouts
                .iter()
                .any(|timeout| match &timeout.request {
                    ExecutionPortMessage::ApprovalRequestMessage(request) => {
                        request.approval_id.0 == id
                    }
                    ExecutionPortMessage::InputRequestMessage(request) => {
                        request.input_request_id.0 == id
                    }
                    _ => false,
                })
        })
    }

    pub(super) fn original_input_request(
        &self,
        generated: InputRequestMessage,
        timed_out: bool,
    ) -> Result<InputRequestMessage, ProductionCodexError> {
        match self
            .outbox
            .retained_interaction(&generated.message_id.0)
            .map_err(map_store_error)?
        {
            Some(ExecutionPortMessage::InputRequestMessage(original))
                if original.input_request_id == generated.input_request_id
                    && original.session_identity == generated.session_identity
                    && original.worker_session_id == generated.worker_session_id
                    && retained_lease_matches_current(&original.lease, &generated.lease) =>
            {
                Ok(original)
            }
            None if !timed_out => Ok(generated),
            _ => Err(conflict()),
        }
    }

    pub(super) fn enqueue_interaction_request(
        &mut self,
        request: ExecutionPortMessage,
    ) -> Result<CodexPoll, ProductionCodexError> {
        let (id, _, identity) = request_identity(&request).ok_or_else(conflict)?;
        let run_key = self
            .run_key_for_thread(&identity.codex_thread_id)?
            .to_owned();
        let run = self.runs.get_mut(&run_key).ok_or_else(unknown_thread)?;
        if let Some(timeout) = run.record.interaction_timeouts.iter_mut().find(|timeout| {
            request_identity(&timeout.request).is_some_and(|(stored, _, _)| stored == id)
        }) {
            if timeout.request != request {
                return Err(conflict());
            }
            // A fresh Core session can replay the exact tool after a crash.
            // Only that exact event re-arms an already-resolved timeout. In
            // particular, an old input answer must not settle a newer input
            // waiter which happens to share the same turn.
            timeout.applied_kernel_session_id = None;
            timeout.waiting_for_exact_event = false;
            run.record
                .recovered_interaction_requests
                .retain(|stored| stored != id);
            self.persist_run(&run_key)?;
            return Ok(CodexPoll::Pending);
        }
        // The exact Core event has recreated this interaction waiter. A retained
        // request from another waiter stays fenced across session recovery.
        let recovered = run
            .record
            .recovered_interaction_requests
            .iter()
            .any(|stored| stored == id);
        if recovered {
            run.record
                .recovered_interaction_requests
                .retain(|stored| stored != id);
            self.persist_run(&run_key)?;
        }
        self.outbox.retain(&request).map_err(map_store_error)?;
        self.action_gate
            .enqueue_message(request)
            .map_err(|_| unavailable())?;
        Ok(CodexPoll::Pending)
    }

    fn pending_interaction_digest(
        &self,
        run_key: &str,
        request: &ExecutionPortMessage,
    ) -> Result<Option<String>, ProductionCodexError> {
        match request {
            ExecutionPortMessage::ApprovalRequestMessage(request) => {
                let operation = self
                    .store
                    .load_approval_operation(&request.approval_id.0)
                    .map_err(map_store_error)?
                    .ok_or_else(conflict)?;
                if operation.run_key != run_key {
                    return Err(conflict());
                }
                Ok((operation.state == StoredApprovalOperationState::Pending)
                    .then_some(operation.request_digest))
            }
            ExecutionPortMessage::InputRequestMessage(request) => {
                let operation = self
                    .store
                    .load_input_operation(&request.input_request_id.0)
                    .map_err(map_store_error)?
                    .ok_or_else(conflict)?;
                if operation.run_key != run_key {
                    return Err(conflict());
                }
                Ok((operation.state == StoredInputOperationState::Pending)
                    .then_some(operation.request_digest))
            }
            _ => Err(conflict()),
        }
    }

    pub(super) async fn settle_interaction_deadlines(
        &mut self,
        run_key: &str,
        now: &Instant,
    ) -> Result<(), ProductionCodexError> {
        if !canonical_instant(now) {
            return Err(conflict());
        }
        self.bridge
            .authority()
            .update_now(now)
            .map_err(map_bridge_error)?;
        self.action_gate
            .update_now(now)
            .map_err(|_| unavailable())?;
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        if run.record.terminal.is_some() {
            return Ok(());
        }
        self.retain_interaction_timeouts(run_key, now)?;
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        let session = run.record.kernel_session_id.clone();
        let timeouts = run.record.interaction_timeouts.clone();
        for (index, timeout) in timeouts.iter().enumerate() {
            if timeout.waiting_for_exact_event {
                continue;
            }
            if timeout.applied_kernel_session_id.as_deref() == Some(session.as_str()) {
                continue;
            }
            // Resolved operations in an older session are historical facts.
            // Reapply only after the exact Core request event re-arms them.
            if timeout.applied_kernel_session_id.is_some()
                && self
                    .pending_interaction_digest(run_key, &timeout.request)?
                    .is_none()
            {
                continue;
            }
            self.apply_interaction_timeout(run_key, &session, timeout)
                .await?;
            #[cfg(feature = "test-support")]
            if self.config.lifecycle_faults.interaction_timeout_exit {
                std::process::exit(73);
            }
            self.finish_interaction_delivery(&timeout.request)?;
            self.runs
                .get_mut(run_key)
                .ok_or_else(unknown_thread)?
                .record
                .interaction_timeouts[index]
                .applied_kernel_session_id = Some(session.clone());
            self.persist_run(run_key)?;
        }
        Ok(())
    }

    fn retain_interaction_timeouts(
        &mut self,
        run_key: &str,
        now: &Instant,
    ) -> Result<(), ProductionCodexError> {
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        let identity = run.binding.authority.session_identity.clone();
        let mut added = false;
        for delivery in self.outbox.pending().map_err(map_store_error)? {
            let Some((id, deadline, owner)) = request_identity(&delivery.message) else {
                continue;
            };
            if owner != &identity {
                continue;
            }
            if !canonical_instant(deadline) {
                return Err(conflict());
            }
            if now.0 < deadline.0 {
                continue;
            }
            if self
                .runs
                .get(run_key)
                .ok_or_else(unknown_thread)?
                .record
                .interaction_timeouts
                .iter()
                .any(|timeout| {
                    request_identity(&timeout.request).is_some_and(|(stored, _, _)| stored == id)
                })
            {
                continue;
            }
            let Some(operation_request_digest) =
                self.pending_interaction_digest(run_key, &delivery.message)?
            else {
                continue;
            };
            let waiting_for_exact_event = self
                .runs
                .get(run_key)
                .ok_or_else(unknown_thread)?
                .record
                .recovered_interaction_requests
                .contains(id);
            let timeout = StoredInteractionTimeout {
                cause_code: "INTERACTION_DEADLINE_EXPIRED".to_owned(),
                request_digest: private_payload_digest(
                    b"winwincode.interaction-timeout-request.v1",
                    &delivery.message,
                )?,
                request: delivery.message,
                operation_request_digest,
                observed_at: now.clone(),
                applied_kernel_session_id: None,
                waiting_for_exact_event,
            };
            let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
            run.record.interaction_timeouts.push(timeout);
            run.record.last_activity_at = now.clone();
            added = true;
        }
        // No Core response is submitted before its immutable timeout intent.
        if added {
            self.persist_run(run_key)?;
        }
        Ok(())
    }

    fn finish_interaction_delivery(
        &self,
        request: &ExecutionPortMessage,
    ) -> Result<(), ProductionCodexError> {
        self.outbox
            .settle_interaction_deadline(request)
            .map_err(map_store_error)?;
        let (id, _, _) = request_identity(request).ok_or_else(conflict)?;
        self.action_gate
            .discard_interaction_message(&id.0)
            .map_err(|_| unavailable())
    }

    fn validate_interaction_timeout(
        &self,
        run_key: &str,
        timeout: &StoredInteractionTimeout,
    ) -> Result<(), ProductionCodexError> {
        let (_, deadline, identity) = request_identity(&timeout.request).ok_or_else(conflict)?;
        if timeout.cause_code != "INTERACTION_DEADLINE_EXPIRED"
            || !canonical_instant(&timeout.observed_at)
            || timeout.observed_at.0 < deadline.0
            || identity
                != &self
                    .runs
                    .get(run_key)
                    .ok_or_else(unknown_thread)?
                    .binding
                    .authority
                    .session_identity
            || timeout.request_digest
                != private_payload_digest(
                    b"winwincode.interaction-timeout-request.v1",
                    &timeout.request,
                )?
        {
            return Err(conflict());
        }
        let (id, _, _) = request_identity(&timeout.request).ok_or_else(conflict)?;
        if self
            .outbox
            .retained_interaction(&id.0)
            .map_err(map_store_error)?
            .as_ref()
            != Some(&timeout.request)
        {
            return Err(conflict());
        }
        Ok(())
    }

    async fn apply_interaction_timeout(
        &mut self,
        run_key: &str,
        session: &str,
        timeout: &StoredInteractionTimeout,
    ) -> Result<(), ProductionCodexError> {
        self.validate_interaction_timeout(run_key, timeout)?;
        let resolution = private_payload_digest(
            b"winwincode.interaction-deadline-expired.v1",
            &timeout.request,
        )?;
        match &timeout.request {
            ExecutionPortMessage::ApprovalRequestMessage(request) => {
                let operation = self
                    .store
                    .load_approval_operation(&request.approval_id.0)
                    .map_err(map_store_error)?
                    .ok_or_else(conflict)?;
                if operation.run_key != run_key
                    || operation.request_digest != timeout.operation_request_digest
                {
                    return Err(conflict());
                }
                if operation.state == StoredApprovalOperationState::Resolved
                    && operation.resolution_digest.as_deref() != Some(resolution.as_str())
                {
                    return Err(conflict());
                }
                self.kernel.resolve_approval(ApprovalResponse {
                    session_id: session.to_owned(),
                    kind: match operation.operation_kind {
                        StoredApprovalOperationKind::Exec => ApprovalKind::Exec,
                        StoredApprovalOperationKind::Patch => ApprovalKind::Patch,
                        StoredApprovalOperationKind::Mcp => ApprovalKind::Mcp,
                    },
                    operation_id: operation.operation_id,
                    turn_id: operation.turn_id,
                    decision: ApprovalDecision::Denied {
                        rejection: "INTERACTION_DEADLINE_EXPIRED: approval expired; continue within the existing permissions".to_owned(),
                    },
                }).await.map_err(|_| kernel_error())?;
                self.store
                    .resolve_approval_operation(
                        &request.approval_id.0,
                        &operation.request_digest,
                        &resolution,
                    )
                    .map_err(map_store_error)?;
            }
            ExecutionPortMessage::InputRequestMessage(request) => {
                let operation = self
                    .store
                    .load_input_operation(&request.input_request_id.0)
                    .map_err(map_store_error)?
                    .ok_or_else(conflict)?;
                if operation.run_key != run_key
                    || operation.request_digest != timeout.operation_request_digest
                {
                    return Err(conflict());
                }
                if operation.state == StoredInputOperationState::Resolved
                    && operation.resolution_digest.as_deref() != Some(resolution.as_str())
                {
                    return Err(conflict());
                }
                let mut answers = HashMap::new();
                answers.insert(
                    operation.question_id,
                    RequestUserInputAnswer {
                        answers: Vec::new(),
                    },
                );
                self.kernel
                    .resolve_user_input(
                        session,
                        operation.turn_id,
                        RequestUserInputResponse { answers },
                    )
                    .await
                    .map_err(|_| kernel_error())?;
                self.store
                    .resolve_input_operation(
                        &request.input_request_id.0,
                        &operation.request_digest,
                        &resolution,
                    )
                    .map_err(map_store_error)?;
            }
            _ => return Err(conflict()),
        }
        Ok(())
    }
}
