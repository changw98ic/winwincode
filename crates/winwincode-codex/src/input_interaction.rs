// SPDX-License-Identifier: Apache-2.0

use super::{
    CodexPoll, InputResponseMessage, Instant, ProductionCodexAdapter, ProductionCodexError,
    RequestUserInputEvent, StoredInputOperation, StoredInputOperationState, canonical_instant,
    canonical_parts_id, conflict, interaction_timeout, interactive_input_choice_replay_keys,
    kernel_error, map_bridge_error, map_store_error, non_empty, private_payload_digest,
    retained_lease_matches_current, unknown_thread, valid_prefixed_id,
};
use codex_protocol::request_user_input::{RequestUserInputAnswer, RequestUserInputResponse};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use winwincode_execution_port::generated::ExecutionPortMessage;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InputQuestionBinding {
    input_request_id: String,
    question_id: String,
}

#[cfg(test)]
#[path = "input_interaction_tests.rs"]
mod tests;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InputAnswerIntent {
    resolution_digest: String,
    values: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InputCallIntent {
    turn_id: String,
    call_id: String,
    questions: Vec<InputQuestionBinding>,
    expires_at: Instant,
    answers: BTreeMap<String, InputAnswerIntent>,
    #[serde(default)]
    ready_incarnation_id: String,
    #[serde(default)]
    applied_incarnation_id: Option<String>,
}

#[derive(Clone, Copy)]
enum InputCallSettlement {
    UserAnswer,
    DeadlineTimeout,
}

impl ProductionCodexAdapter {
    pub(super) fn accept_input_request_event(
        &mut self,
        run_key: &str,
        request: &RequestUserInputEvent,
    ) -> Result<CodexPoll, ProductionCodexError> {
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        let session = run.record.kernel_session_id.clone();
        let turn_id = non_empty(request.turn_id.clone()).ok_or_else(conflict)?;
        let digest = private_payload_digest(b"winwincode.kernel-input-request.v1", request)?;
        let incarnation = private_payload_digest(
            b"winwincode.input-waiter-incarnation.v1",
            &[
                run.input_waiter_incarnation.as_str(),
                session.as_str(),
                turn_id.as_str(),
                request.call_id.as_str(),
                digest.as_str(),
            ],
        )?;
        let mut question_ids = HashSet::new();
        if request.call_id.trim().is_empty()
            || request.questions.is_empty()
            || request.questions.len() > 3
            || request
                .questions
                .iter()
                .any(|question| question.id.trim().is_empty() || !question_ids.insert(&question.id))
        {
            return Err(conflict());
        }
        let questions = request
            .questions
            .iter()
            .map(|question| InputQuestionBinding {
                input_request_id: canonical_parts_id(
                    "inp",
                    b"winwincode-kernel-input.v1",
                    &[
                        run_key.as_bytes(),
                        turn_id.as_bytes(),
                        request.call_id.as_bytes(),
                        question.id.as_bytes(),
                    ],
                ),
                question_id: question.id.clone(),
            })
            .collect::<Vec<_>>();
        self.validate_input_question_replays(run_key, request, &turn_id, &digest, &questions)?;
        let proposed = InputCallIntent {
            turn_id,
            call_id: request.call_id.clone(),
            questions,
            expires_at: self
                .current_binding_for_run(run_key)?
                .authority
                .lease
                .expires_at
                .clone(),
            answers: BTreeMap::new(),
            ready_incarnation_id: incarnation.clone(),
            applied_incarnation_id: None,
        };
        let mut call: InputCallIntent = self
            .store
            .retain_input_call(run_key, &digest, &proposed)
            .map_err(map_store_error)?;
        if call.turn_id != proposed.turn_id
            || call.call_id != proposed.call_id
            || call.questions != proposed.questions
        {
            return Err(conflict());
        }
        if call.ready_incarnation_id != incarnation {
            let previous = call.clone();
            call.ready_incarnation_id = incarnation;
            self.store
                .replace_input_call(run_key, &digest, &previous, &call)
                .map_err(map_store_error)?;
        }
        for question in &call.questions {
            if let Some(answer) = call.answers.get(&question.input_request_id) {
                self.store
                    .resolve_input_operation(
                        &question.input_request_id,
                        &digest,
                        &answer.resolution_digest,
                    )
                    .map_err(map_store_error)?;
            }
        }
        for question in &request.questions {
            if let Some(message) =
                self.retain_input_question(run_key, request, question, &call.expires_at)?
            {
                self.enqueue_interaction_request(ExecutionPortMessage::InputRequestMessage(
                    message,
                ))?;
            }
        }
        Ok(CodexPoll::Pending)
    }

    fn validate_input_question_replays(
        &self,
        run_key: &str,
        request: &RequestUserInputEvent,
        turn_id: &str,
        digest: &str,
        questions: &[InputQuestionBinding],
    ) -> Result<(), ProductionCodexError> {
        for (question, binding) in request.questions.iter().zip(questions) {
            let choices = interactive_input_choice_replay_keys(question)?;
            if question.is_other && choices.as_ref().is_none_or(Vec::is_empty) {
                return Err(conflict());
            }
            if let Some(existing) = self
                .store
                .load_input_operation(&binding.input_request_id)
                .map_err(map_store_error)?
                && (existing.run_key != run_key
                    || existing.turn_id != turn_id
                    || existing.question_id != question.id
                    || existing.request_digest != digest)
            {
                return Err(conflict());
            }
        }
        Ok(())
    }

    async fn replay_resolved_input_response_exact(
        &mut self,
        response: &InputResponseMessage,
        operation: &StoredInputOperation,
        received_at: &Instant,
    ) -> Result<(), ProductionCodexError> {
        if response.sent_at != response.responded_at
            || !canonical_instant(&response.responded_at)
            || !canonical_instant(received_at)
            || response.responded_at.0 > received_at.0
            || !valid_prefixed_id(&response.input_request_id.0, "inp_")
            || !valid_prefixed_id(&response.message_id.0, "xmsg_")
        {
            return Err(conflict());
        }
        let resolution_digest = private_payload_digest(b"winwincode.input-response.v1", response)?;
        if operation.resolution_digest.as_deref() != Some(resolution_digest.as_str()) {
            return Err(conflict());
        }
        let original = self.retained_input_request(operation)?;
        let value = interaction_timeout::input_response_value(response, &original)?;
        let call: InputCallIntent = self
            .store
            .load_input_call(&operation.run_key, &operation.request_digest)
            .map_err(map_store_error)?
            .ok_or_else(conflict)?;
        let answer = InputAnswerIntent {
            resolution_digest,
            values: value.into_iter().collect(),
        };
        if call.turn_id != operation.turn_id
            || call.answers.get(&operation.input_request_id) != Some(&answer)
            || !call.questions.iter().any(|question| {
                question.input_request_id == operation.input_request_id
                    && question.question_id == operation.question_id
            })
        {
            return Err(conflict());
        }
        self.outbox
            .record_applied_response(&ExecutionPortMessage::InputResponseMessage(
                response.clone(),
            ))
            .map_err(map_store_error)?;
        // A valid replay can arrive after expiry while the Adapter's
        // last observation still predates it. Observe receipt time before
        // considering consumption by a live Core waiter.
        self.bridge
            .authority()
            .update_now(received_at)
            .map_err(map_bridge_error)?;
        // Durable acceptance of one answer does not prove consumption of
        // the complete multi-question call by its current exact waiter.
        if self.runs.contains_key(&operation.run_key) {
            self.apply_complete_input_call(&operation.run_key, &operation.request_digest)
                .await?;
        }
        Ok(())
    }

    pub(super) async fn accept_input_response_exact(
        &mut self,
        response: &InputResponseMessage,
        received_at: &Instant,
    ) -> Result<(), ProductionCodexError> {
        let operation = self
            .store
            .load_input_operation(&response.input_request_id.0)
            .map_err(map_store_error)?
            .ok_or_else(conflict)?;
        if operation.state == StoredInputOperationState::Resolved {
            return self
                .replay_resolved_input_response_exact(response, &operation, received_at)
                .await;
        }
        let authority = self.current_binding_for_run(&operation.run_key)?.authority;
        if !retained_lease_matches_current(&response.lease, &authority.lease)
            || response.worker_session_id != authority.worker_session_id
            || response.session_identity != authority.session_identity
            || response.sent_at != response.responded_at
            || !canonical_instant(&response.responded_at)
            || !canonical_instant(received_at)
            || response.responded_at.0 < authority.lease.issued_at.0
            || response.responded_at.0 > received_at.0
            || response.responded_at.0 >= authority.lease.expires_at.0
            || received_at.0 < authority.lease.issued_at.0
            || received_at.0 >= authority.lease.expires_at.0
            || !valid_prefixed_id(&response.input_request_id.0, "inp_")
            || !valid_prefixed_id(&response.message_id.0, "xmsg_")
        {
            return Err(conflict());
        }
        let original = self.retained_input_request(&operation)?;
        let value = interaction_timeout::input_response_value(response, &original)?;
        let resolution_digest = private_payload_digest(b"winwincode.input-response.v1", response)?;
        let mut call: InputCallIntent = self
            .store
            .load_input_call(&operation.run_key, &operation.request_digest)
            .map_err(map_store_error)?
            .ok_or_else(conflict)?;
        if call.turn_id != operation.turn_id
            || !call.questions.iter().any(|question| {
                question.input_request_id == operation.input_request_id
                    && question.question_id == operation.question_id
            })
        {
            return Err(conflict());
        }
        self.ensure_input_deadline(&operation, response, received_at)
            .await?;
        if received_at.0 >= call.expires_at.0 || response.responded_at.0 >= call.expires_at.0 {
            return Err(conflict());
        }
        let answer = InputAnswerIntent {
            resolution_digest: resolution_digest.clone(),
            values: value.into_iter().collect(),
        };
        if let Some(existing) = call.answers.get(&operation.input_request_id)
            && existing != &answer
        {
            return Err(conflict());
        }
        self.bridge
            .authority()
            .update_now(received_at)
            .map_err(map_bridge_error)?;
        let previous = call.clone();
        if !call.answers.contains_key(&operation.input_request_id) {
            call.answers
                .insert(operation.input_request_id.clone(), answer);
        }
        self.store
            .resolve_input_call_answer(&operation, &resolution_digest, &previous, &call)
            .map_err(map_store_error)?;
        self.outbox
            .record_applied_response(&ExecutionPortMessage::InputResponseMessage(
                response.clone(),
            ))
            .map_err(map_store_error)?;
        self.apply_complete_input_call(&operation.run_key, &operation.request_digest)
            .await?;
        let run = self
            .runs
            .get_mut(&operation.run_key)
            .ok_or_else(unknown_thread)?;
        run.record.last_activity_at = received_at.clone();
        self.persist_run(&operation.run_key)
    }

    pub(super) async fn apply_complete_input_call(
        &mut self,
        run_key: &str,
        digest: &str,
    ) -> Result<(), ProductionCodexError> {
        self.apply_input_call(run_key, digest, InputCallSettlement::UserAnswer)
            .await
    }

    async fn apply_input_call(
        &mut self,
        run_key: &str,
        digest: &str,
        settlement: InputCallSettlement,
    ) -> Result<(), ProductionCodexError> {
        let mut call: InputCallIntent = self
            .store
            .load_input_call(run_key, digest)
            .map_err(map_store_error)?
            .ok_or_else(conflict)?;
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        if !run.kernel_live || run.record.operation_fenced_at.is_some() {
            return Ok(());
        }
        let source = self.bridge.authority();
        let Some(binding) = source
            .binding_for_alias(&run.binding.canonical_thread_id.0)
            .map_err(map_bridge_error)?
        else {
            return Ok(());
        };
        let now = source
            .observed_now()
            .map_err(map_bridge_error)?
            .ok_or_else(conflict)?;
        if binding.run_key != run_key
            || binding.kernel_session_id != run.record.kernel_session_id
            || binding.authority != run.binding.authority
            || now.0 < binding.authority.lease.issued_at.0
            || now.0 >= binding.authority.lease.expires_at.0
            || (matches!(settlement, InputCallSettlement::UserAnswer) && now.0 >= call.expires_at.0)
        {
            return Ok(());
        }
        let session = run.record.kernel_session_id.clone();
        let incarnation = private_payload_digest(
            b"winwincode.input-waiter-incarnation.v1",
            &[
                run.input_waiter_incarnation.as_str(),
                session.as_str(),
                call.turn_id.as_str(),
                call.call_id.as_str(),
                digest,
            ],
        )?;
        if call.answers.len() != call.questions.len()
            || call.ready_incarnation_id != incarnation
            || call.applied_incarnation_id.as_deref() == Some(incarnation.as_str())
        {
            return Ok(());
        }
        let mut answers = HashMap::new();
        for question in &call.questions {
            let answer = call
                .answers
                .get(&question.input_request_id)
                .ok_or_else(conflict)?;
            let operation = self
                .store
                .load_input_operation(&question.input_request_id)
                .map_err(map_store_error)?
                .ok_or_else(conflict)?;
            let original = self.retained_input_request(&operation)?;
            interaction_timeout::validate_retained_input_answers(&original, &answer.values)?;
            answers.insert(
                question.question_id.clone(),
                RequestUserInputAnswer {
                    answers: answer.values.clone(),
                },
            );
        }
        self.kernel
            .resolve_user_input(
                &session,
                call.turn_id.clone(),
                call.call_id.clone(),
                RequestUserInputResponse { answers },
            )
            .await
            .map_err(|_| kernel_error())?;
        let previous = call.clone();
        call.applied_incarnation_id = Some(incarnation);
        self.store
            .replace_input_call(run_key, digest, &previous, &call)
            .map_err(map_store_error)
    }

    pub(super) async fn apply_input_timeout_answer(
        &mut self,
        operation: &StoredInputOperation,
        resolution_digest: &str,
    ) -> Result<(), ProductionCodexError> {
        let mut call: InputCallIntent = self
            .store
            .load_input_call(&operation.run_key, &operation.request_digest)
            .map_err(map_store_error)?
            .ok_or_else(conflict)?;
        let previous = call.clone();
        if let Some(answer) = call.answers.get(&operation.input_request_id) {
            if answer.resolution_digest != resolution_digest {
                return Err(conflict());
            }
        } else {
            call.answers.insert(
                operation.input_request_id.clone(),
                InputAnswerIntent {
                    resolution_digest: resolution_digest.to_owned(),
                    values: Vec::new(),
                },
            );
        }
        self.store
            .resolve_input_call_answer(operation, resolution_digest, &previous, &call)
            .map_err(map_store_error)?;
        // The original immutable deadline authorizes this empty timeout answer.
        // User answers keep their deadline fence; both paths still require the
        // live renewed lease and the exact current Core waiter incarnation.
        self.apply_input_call(
            &operation.run_key,
            &operation.request_digest,
            InputCallSettlement::DeadlineTimeout,
        )
        .await
    }
}
