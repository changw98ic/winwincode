// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::CodexCoreAdapter;
use crate::adapter::tests::{delegated_record_and_binding, diagnostic_adapter_config};
use crate::adapter::{ActiveRun, OperationDelivery};
use codex_protocol::request_user_input::{
    RequestUserInputQuestion, RequestUserInputQuestionOption,
};

fn retain_closed_input_prompt_fixture(
    adapter: &mut ProductionCodexAdapter,
    root: &std::path::Path,
) -> (
    String,
    winwincode_execution_port::generated::InputRequestMessage,
) {
    let (mut record, mut binding) = delegated_record_and_binding();
    record.role_policy = None;
    record.workspace = root.join("workspace");
    std::fs::create_dir_all(&record.workspace).unwrap();
    binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
    binding.authority.lease.expires_at = Instant("2026-08-28T00:15:00.000Z".into());
    let key = binding.run_key.clone();
    adapter
        .install_active_run(&key, record, binding, true, false)
        .unwrap();
    let request = RequestUserInputEvent {
        call_id: "closed-typed-worker-answer".into(),
        turn_id: "turn-fixture".into(),
        is_blocking: true,
        auto_resolution_ms: None,
        questions: (0..2)
            .map(|index| RequestUserInputQuestion {
                id: format!("question-{index}"),
                header: "Choice".into(),
                question: "Choose?".into(),
                is_other: false,
                is_secret: false,
                options: Some(vec![
                    RequestUserInputQuestionOption {
                        label: "continue".into(),
                        description: "Continue".into(),
                    },
                    RequestUserInputQuestionOption {
                        label: "revise".into(),
                        description: "Revise".into(),
                    },
                ]),
            })
            .collect(),
    };
    adapter.accept_input_request_event(&key, &request).unwrap();
    let prompt = adapter
        .outbox
        .pending()
        .unwrap()
        .into_iter()
        .find_map(|delivery| match delivery.message {
            ExecutionPortMessage::InputRequestMessage(prompt) => Some(prompt),
            _ => None,
        })
        .unwrap();
    (key, prompt)
}

#[tokio::test]
async fn closed_input_prompt_rejects_unknown_typed_worker_value_before_durable_acceptance() {
    use winwincode_execution_port::generated::{
        InputResponseMessageKind, InputResponseMessageStatus,
    };

    let root =
        std::env::temp_dir().join(format!("wwc-input-closed-answer-{}", uuid::Uuid::new_v4()));
    let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    let (key, prompt) = retain_closed_input_prompt_fixture(&mut adapter, &root);
    let received_at = Instant("2026-08-28T00:01:00.000Z".into());
    let response = InputResponseMessage {
        input_request_id: prompt.input_request_id.clone(),
        kind: InputResponseMessageKind::InputResponse,
        lease: prompt.lease.clone(),
        message_id: winwincode_domain::ExecutionMessageId("xmsg_00000000000000000000000100".into()),
        responded_at: received_at.clone(),
        schema_version: prompt.schema_version.clone(),
        sent_at: received_at.clone(),
        session_identity: prompt.session_identity.clone(),
        status: InputResponseMessageStatus::Provided,
        value: Some(winwincode_domain::InteractiveInputValue {
            mode: prompt.mode.clone(),
            value: "value-outside-original-closed-choice-set".into(),
        }),
        worker_session_id: prompt.worker_session_id.clone(),
    };
    for (index, mode, value, status) in [
        (
            0,
            prompt.mode.clone(),
            "value-outside-original-closed-choice-set".into(),
            InputResponseMessageStatus::Provided,
        ),
        (
            1,
            winwincode_domain::InteractiveInputMode::Text,
            "continue".into(),
            InputResponseMessageStatus::Provided,
        ),
        (
            2,
            prompt.mode.clone(),
            "🦀".repeat(20_001),
            InputResponseMessageStatus::Provided,
        ),
        (
            3,
            prompt.mode.clone(),
            "  \n\t".into(),
            InputResponseMessageStatus::Provided,
        ),
        (
            4,
            prompt.mode.clone(),
            "continue".into(),
            InputResponseMessageStatus::Cancelled,
        ),
        (
            5,
            prompt.mode.clone(),
            "continue".into(),
            InputResponseMessageStatus::Expired,
        ),
    ] {
        let mut invalid = response.clone();
        invalid.message_id.0 = format!("xmsg_{:026}", 100 + index);
        invalid.status = status;
        invalid.value = Some(winwincode_domain::InteractiveInputValue { mode, value });
        let accepted = adapter
            .accept_input_response_exact(&invalid, &received_at)
            .await;
        let operation = adapter
            .store
            .load_input_operation(&prompt.input_request_id.0)
            .unwrap()
            .unwrap();
        let call: InputCallIntent = adapter
            .store
            .load_input_call(&key, &operation.request_digest)
            .unwrap()
            .unwrap();
        eprintln!(
            "CLOSED_INPUT_BOUNDARY case={} accepted={} operationState={:?} durableAnswers={}",
            index,
            accepted.is_ok(),
            operation.state,
            call.answers.len()
        );
        assert!(
            accepted.is_err(),
            "invalid typed Worker value must be rejected before durable acceptance"
        );
        assert_eq!(operation.state, StoredInputOperationState::Pending);
        assert!(call.answers.is_empty());
        assert!(call.applied_incarnation_id.is_none());
    }
    drop(adapter);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_closed_input_projection_preserves_serialized_contract() {
    let root =
        std::env::temp_dir().join(format!("wwc-input-other-producer-{}", uuid::Uuid::new_v4()));
    let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    let (mut record, mut binding) = delegated_record_and_binding();
    binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
    binding.authority.lease.expires_at = Instant("2026-08-28T00:15:00.000Z".into());
    record.role_policy = None;
    record.workspace = root.join("workspace");
    std::fs::create_dir_all(&record.workspace).unwrap();
    let key = binding.run_key.clone();
    adapter
        .install_active_run(&key, record, binding, true, false)
        .unwrap();
    let request = RequestUserInputEvent {
        call_id: "public-other-permission".into(),
        turn_id: "turn-fixture".into(),
        is_blocking: true,
        auto_resolution_ms: None,
        questions: [false, false]
            .into_iter()
            .enumerate()
            .map(|(index, is_other)| RequestUserInputQuestion {
                id: format!("question-{index}"),
                header: "Choice".into(),
                question: "Choose?".into(),
                is_other,
                is_secret: false,
                options: Some(vec![
                    RequestUserInputQuestionOption {
                        label: "continue".into(),
                        description: "Continue".into(),
                    },
                    RequestUserInputQuestionOption {
                        label: "revise".into(),
                        description: "Revise".into(),
                    },
                ]),
            })
            .collect(),
    };
    adapter.accept_input_request_event(&key, &request).unwrap();
    let prompts = adapter
        .outbox
        .pending()
        .unwrap()
        .into_iter()
        .filter_map(|delivery| match delivery.message {
            ExecutionPortMessage::InputRequestMessage(prompt) => Some(prompt),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 2);
    let flags = prompts
        .iter()
        .map(|prompt| {
            let encoded = serde_json::to_value(prompt).unwrap();
            encoded.get("allowOther").cloned()
        })
        .collect::<Vec<_>>();
    assert!(flags.iter().all(Option::is_none));
    let encoded_before = prompts
        .iter()
        .map(serde_json::to_vec)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    adapter.accept_input_request_event(&key, &request).unwrap();
    let encoded_after = adapter
        .outbox
        .pending()
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry.message {
            ExecutionPortMessage::InputRequestMessage(prompt) => {
                Some(serde_json::to_vec(&prompt).unwrap())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        encoded_after, encoded_before,
        "exact retained legacy frames keep their bytes and omitted permission"
    );
    drop(adapter);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn empty_core_input_turn_never_falls_back_or_writes_a_call_or_question() {
    let root = std::env::temp_dir().join(format!("wwc-input-exact-turn-{}", uuid::Uuid::new_v4()));
    let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    let (record, binding) = delegated_record_and_binding();
    let run_key = binding.run_key.clone();
    let database = adapter
        .config
        .data_directory
        .join(crate::store::DATABASE_FILE);
    assert!(
        record.current_turn_id.is_some(),
        "fixture provides the unsafe fallback target"
    );
    adapter.runs.insert(
        run_key.clone(),
        ActiveRun {
            input_waiter_incarnation: uuid::Uuid::new_v4().to_string(),
            record,
            binding,
            post_action_trace_positions: HashMap::new(),
            replay: std::collections::VecDeque::new(),
            kernel_live: false,
            kernel_close_pending: false,
            recovered: false,
            batch_intent_emission: OperationDelivery::default(),
            format_repair_reconciliation: OperationDelivery::default(),
            pending_fusion: None,
        },
    );
    for turn_id in ["", "   "] {
        let event = RequestUserInputEvent {
            call_id: "exact-input-call".into(),
            turn_id: turn_id.into(),
            is_blocking: true,
            auto_resolution_ms: None,
            questions: vec![RequestUserInputQuestion {
                id: "same-question".into(),
                header: "Choice".into(),
                question: "Choose?".into(),
                is_other: false,
                is_secret: false,
                options: Some(vec![
                    RequestUserInputQuestionOption {
                        label: "continue".into(),
                        description: "Continue".into(),
                    },
                    RequestUserInputQuestionOption {
                        label: "revise".into(),
                        description: "Revise".into(),
                    },
                ]),
            }],
        };
        assert!(
            adapter
                .accept_input_request_event(&run_key, &event)
                .is_err()
        );
        let connection = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        for table in ["input_call", "input_operation"] {
            let rows: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                rows, 0,
                "invalid exact turn cannot retain partial interaction state"
            );
        }
        assert!(adapter.outbox.pending().unwrap().is_empty());
    }
    drop(adapter);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "The exact durable-write fault, reopen, renewal and original-event replay stay in one offline fixture."
)]
fn durable_input_call_without_first_prompt_keeps_original_deadline_after_renewal() {
    use winwincode_execution_port::generated::{LeaseRenewMessage, LeaseRenewMessageKind};

    let root =
        std::env::temp_dir().join(format!("wwc-input-missing-prompt-{}", uuid::Uuid::new_v4()));
    let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    let (mut record, mut binding) = delegated_record_and_binding();
    record.role_policy = None;
    record.workspace = root.join("workspace");
    std::fs::create_dir_all(&record.workspace).unwrap();
    let original_deadline = Instant("2026-08-28T00:15:00.000Z".into());
    binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
    binding.authority.lease.expires_at = original_deadline.clone();
    binding.authority.lease.lease_id.0 = "lse_00000000000000000000000001".into();
    binding.authority.lease.fencing_token.0 = "1".into();
    binding.opened_at = binding.authority.lease.issued_at.clone();
    record.last_activity_at = binding.authority.lease.issued_at.clone();
    let key = binding.run_key.clone();
    let thread = binding.canonical_thread_id.clone();
    adapter
        .install_active_run(&key, record, binding.clone(), true, false)
        .unwrap();
    let database = adapter
        .config
        .data_directory
        .join(crate::store::DATABASE_FILE);
    let request = RequestUserInputEvent {
        call_id: "durable-input-before-first-prompt".into(),
        turn_id: "turn-fixture".into(),
        is_blocking: true,
        auto_resolution_ms: None,
        questions: vec![RequestUserInputQuestion {
            id: "first-question".into(),
            header: "Choice".into(),
            question: "Choose?".into(),
            is_other: false,
            is_secret: false,
            options: Some(vec![
                RequestUserInputQuestionOption {
                    label: "continue".into(),
                    description: "Continue".into(),
                },
                RequestUserInputQuestionOption {
                    label: "revise".into(),
                    description: "Revise".into(),
                },
            ]),
        }],
    };
    let digest = private_payload_digest(b"winwincode.kernel-input-request.v1", &request).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_first_input_operation BEFORE INSERT ON input_operation
         BEGIN SELECT RAISE(ABORT, 'isolated crash before first prompt'); END;",
        )
        .unwrap();
    assert!(adapter.accept_input_request_event(&key, &request).is_err());
    let call: InputCallIntent = adapter
        .store
        .load_input_call(&key, &digest)
        .unwrap()
        .unwrap();
    assert_eq!(
        call.expires_at, original_deadline,
        "the original Core call is already durable"
    );
    let operations: i64 = connection
        .query_row("SELECT COUNT(*) FROM input_operation", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        operations, 0,
        "the fault occurs before the first canonical operation"
    );
    assert!(
        adapter.outbox.pending().unwrap().is_empty(),
        "the first product prompt has not been retained"
    );
    drop(connection);
    drop(adapter);

    let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch("DROP TRIGGER fail_first_input_operation")
        .unwrap();
    drop(connection);
    let record = crate::adapter::load_stored_run(&adapter.store, &key)
        .unwrap()
        .unwrap();
    adapter
        .install_active_run(&key, record, binding.clone(), true, true)
        .unwrap();
    let renewal_at = Instant("2026-08-28T00:10:00.000Z".into());
    let mut renewed = binding.authority.lease.clone();
    renewed.expires_at = Instant("2026-08-28T00:30:00.000Z".into());
    let renewal = LeaseRenewMessage {
        kind: LeaseRenewMessageKind::LeaseRenew,
        lease: renewed.clone(),
        prior_expires_at: original_deadline.clone(),
        message_id: winwincode_domain::ExecutionMessageId("xmsg_00000000000000000000000001".into()),
        request_id: winwincode_domain::RequestId("req_00000000000000000000000001".into()),
        schema_version: winwincode_domain::SchemaVersion::WinwincodeV1,
        sent_at: renewal_at.clone(),
    };
    assert!(
        adapter.renew_lease(&thread, &renewal, &renewal_at).unwrap(),
        "the real Adapter renewal path accepts the live same-lease extension"
    );
    assert_eq!(adapter.runs[&key].binding.authority.lease, renewed);
    adapter.accept_input_request_event(&key, &request).unwrap();
    let prompts = adapter
        .outbox
        .pending()
        .unwrap()
        .into_iter()
        .filter_map(|delivery| match delivery.message {
            ExecutionPortMessage::InputRequestMessage(prompt) => Some(prompt),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 1);
    assert_eq!(
        prompts[0].lease, renewed,
        "the prompt keeps the current outer authority identity"
    );
    let call: InputCallIntent = adapter
        .store
        .load_input_call(&key, &digest)
        .unwrap()
        .unwrap();
    assert_eq!(call.expires_at, original_deadline);
    assert_eq!(
        prompts[0].expires_at, call.expires_at,
        "a recovered first prompt must not advertise a later deadline than its durable call accepts"
    );
    let received_at = Instant("2026-08-28T00:16:00.000Z".into());
    let response = InputResponseMessage {
        input_request_id: prompts[0].input_request_id.clone(),
        kind: winwincode_execution_port::generated::InputResponseMessageKind::InputResponse,
        lease: renewed,
        message_id: winwincode_domain::ExecutionMessageId("xmsg_00000000000000000000000110".into()),
        responded_at: received_at.clone(),
        schema_version: prompts[0].schema_version.clone(),
        sent_at: received_at.clone(),
        session_identity: prompts[0].session_identity.clone(),
        status: winwincode_execution_port::generated::InputResponseMessageStatus::Provided,
        value: Some(winwincode_domain::InteractiveInputValue {
            mode: prompts[0].mode.clone(),
            value: "continue".into(),
        }),
        worker_session_id: prompts[0].worker_session_id.clone(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        runtime
            .block_on(adapter.accept_input_response_exact(&response, &received_at))
            .is_err()
    );
    let after: InputCallIntent = adapter
        .store
        .load_input_call(&key, &digest)
        .unwrap()
        .unwrap();
    assert_eq!(after.expires_at, original_deadline);
    assert!(
        after
            .answers
            .values()
            .all(|answer| answer.values.is_empty()),
        "deadline handling retains history and cannot deliver the late provided value"
    );
    drop(adapter);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn retained_legacy_input_values_keep_closed_permissions_and_validate_before_core_submission() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let raw = fixture["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["kind"] == "input.request")
        .unwrap()
        .clone();
    let prompt: winwincode_execution_port::generated::InputRequestMessage =
        serde_json::from_value(raw.clone()).unwrap();
    assert!(raw.get("allowOther").is_none());
    assert!(prompt.allow_other.is_none());
    let legal = prompt.choices.as_ref().unwrap()[0].value.clone();
    assert!(interaction_timeout::validate_retained_input_answers(&prompt, &[legal]).is_ok());
    assert!(interaction_timeout::validate_retained_input_answers(&prompt, &[]).is_ok());
    for values in [
        vec!["unlisted".into()],
        vec!["🦀".repeat(20_001)],
        vec!["first".into(), "second".into()],
    ] {
        assert!(interaction_timeout::validate_retained_input_answers(&prompt, &values).is_err());
    }
    assert_eq!(
        serde_json::to_value(&prompt).unwrap(),
        raw,
        "optional permission does not rewrite legacy retained frames"
    );
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "real retained prompt, historical answer CAS and pre-Core guard are one recovery boundary"
)]
async fn retained_legacy_answer_intent_is_denied_before_core_without_rewriting_history() {
    for invalid_values in [
        vec!["unlisted legacy answer".into()],
        vec!["🦀".repeat(20_001)],
        vec!["first".into(), "second".into()],
    ] {
        let root =
            std::env::temp_dir().join(format!("wwc-input-legacy-answers-{}", uuid::Uuid::new_v4()));
        let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
        let (mut record, mut binding) = delegated_record_and_binding();
        record.role_policy = None;
        record.workspace = root.join("workspace");
        std::fs::create_dir_all(&record.workspace).unwrap();
        binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
        binding.authority.lease.expires_at = Instant("2026-08-28T00:15:00.000Z".into());
        let key = binding.run_key.clone();
        adapter
            .install_active_run(&key, record, binding, true, false)
            .unwrap();
        let request = RequestUserInputEvent {
            call_id: "closed-typed-worker-answer".into(),
            turn_id: "turn-fixture".into(),
            is_blocking: true,
            auto_resolution_ms: None,
            questions: (0..2)
                .map(|index| RequestUserInputQuestion {
                    id: format!("question-{index}"),
                    header: "Choice".into(),
                    question: "Choose?".into(),
                    is_other: false,
                    is_secret: false,
                    options: Some(vec![
                        RequestUserInputQuestionOption {
                            label: "continue".into(),
                            description: "Continue".into(),
                        },
                        RequestUserInputQuestionOption {
                            label: "revise".into(),
                            description: "Revise".into(),
                        },
                    ]),
                })
                .collect(),
        };
        adapter.accept_input_request_event(&key, &request).unwrap();
        let prompt = adapter
            .outbox
            .pending()
            .unwrap()
            .into_iter()
            .find_map(|delivery| match delivery.message {
                ExecutionPortMessage::InputRequestMessage(prompt) => Some(prompt),
                _ => None,
            })
            .unwrap();

        assert!(prompt.allow_other.is_none());
        let digest =
            private_payload_digest(b"winwincode.kernel-input-request.v1", &request).unwrap();
        let mut call: InputCallIntent = adapter
            .store
            .load_input_call(&key, &digest)
            .unwrap()
            .unwrap();
        let questions = call.questions.clone();
        for (index, question) in questions.iter().enumerate() {
            let operation = adapter
                .store
                .load_input_operation(&question.input_request_id)
                .unwrap()
                .unwrap();
            let previous = call.clone();
            let values = if index == 0 {
                invalid_values.clone()
            } else {
                vec!["continue".into()]
            };
            let resolution_digest =
                private_payload_digest(b"legacy-input-fixture-answer.v1", &values).unwrap();
            call.answers.insert(
                question.input_request_id.clone(),
                InputAnswerIntent {
                    resolution_digest: resolution_digest.clone(),
                    values,
                },
            );
            adapter
                .store
                .resolve_input_call_answer(&operation, &resolution_digest, &previous, &call)
                .unwrap();
        }
        assert_eq!(call.answers.len(), call.questions.len());
        assert!(call.applied_incarnation_id.is_none());
        let historical_bytes = serde_json::to_vec(&call).unwrap();
        let error = adapter
            .apply_complete_input_call(&key, &digest)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            conflict(),
            "invalid historical value is rejected by the retained-prompt guard before any Core submission"
        );
        let after: InputCallIntent = adapter
            .store
            .load_input_call(&key, &digest)
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::to_vec(&after).unwrap(), historical_bytes);
        for question in &questions {
            let operation = adapter
                .store
                .load_input_operation(&question.input_request_id)
                .unwrap()
                .unwrap();
            assert_eq!(operation.state, StoredInputOperationState::Resolved);
            assert_eq!(
                operation.resolution_digest.as_ref().unwrap(),
                &after.answers[&question.input_request_id].resolution_digest
            );
        }
        let mut new_projection = prompt.clone();
        new_projection.allow_other = Some(true);
        let retained = adapter
            .original_input_request(new_projection, false)
            .unwrap();
        assert_eq!(
            serde_json::to_vec(&retained).unwrap(),
            serde_json::to_vec(&prompt).unwrap()
        );
        assert!(
            retained.allow_other.is_none(),
            "new producer fields cannot upgrade the retained original prompt"
        );
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }
}
