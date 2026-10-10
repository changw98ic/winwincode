// SPDX-License-Identifier: Apache-2.0

use super::*;

#[allow(
    clippy::too_many_lines,
    reason = "保持合法多问题从真实Core事件到Adapter投影的单fixture路径及原断言"
)]
fn multiquestion_case(mode: &str, count: usize, exact: &str) {
    if std::env::var_os("WWC_MECHANISM_MULTIQ_CHILD").is_none() {
        let root = Fixture::new();
        let barrier = root.0.join("mechanism-barrier");
        fs::create_dir_all(&barrier).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                exact,
                "--include-ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("WWC_MECHANISM_MULTIQ_CHILD", "1")
            .env("WWC_APPROVAL_RESTART_DIRECTORY", &root.0)
            .env("WWC_MECHANISM_CORE_MODE", mode)
            .env("WWC_MECHANISM_INTERACTION_BARRIER", &barrier)
            .output()
            .unwrap();
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "legal {count}-question {mode} request must not end the Job as infrastructure error"
        );
        return;
    }
    let mode = mode.to_owned();
    run_native(async move {
        let root = Fixture::new();
        let dispatch = input_dispatch(&root);
        let config = worker_config(&dispatch);
        let port = RecordedPort::default();
        let mut worker = winwincode_worker::WorkerMain::new(
            config.clone(),
            port.clone(),
            root.adapter_with_owner(&config, true),
            root.workspace_runtime(),
        );
        register(&mut worker, &port, &config).await;
        worker
            .accept_control(
                &wire::ExecutionPortMessage::JobDispatchMessage(dispatch),
                now(),
            )
            .await
            .unwrap();
        let open = wait_for_open(&mut worker, &port, 0).await;
        let questions=(0..count).map(|index|serde_json::json!({"id":format!("choice_{index}"),"header":format!("Choice{index}"),"question":format!("Select choice {index}?"),"options":[{"label":"continue","description":"Continue with this choice."},{"label":"revise","description":"Revise this choice."}]})).collect::<Vec<_>>();
        let arguments = serde_json::json!({"questions":questions});
        let frames = [
            serde_json::json!({"type":"created"}),
            serde_json::json!({"type":"output_item_done","item":{"type":"function_call","name":"request_user_input","namespace":"functions","call_id":"multiquestion-call","arguments":arguments.to_string()}}),
            serde_json::json!({"type":"completed","responseId":"multiquestion-response","tokenUsage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15},"endTurn":false}),
        ];
        deliver_frames_at(&mut worker, &open, 0, &frames, false, now()).await;
        let mut poll_error_count = 0;
        let mut first_poll_error = None;
        let post_frame_started = std::time::Instant::now();
        for _ in 0..400 {
            if let Err(error) = Box::pin(worker.poll_codex(now())).await {
                poll_error_count += 1;
                first_poll_error.get_or_insert_with(|| format!("{error:?}"));
            }
            worker.flush_durable_outbox().await.unwrap();
            if port.messages().iter().any(|message| {
                matches!(
                    message,
                    wire::ExecutionPortMessage::InputRequestMessage(_)
                        | wire::ExecutionPortMessage::JobOutcomeMessage(_)
                )
            }) || post_frame_started.elapsed() >= std::time::Duration::from_secs(20)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let barrier = PathBuf::from(std::env::var_os("WWC_MECHANISM_INTERACTION_BARRIER").unwrap());
        let actual_event: serde_json::Value = serde_json::from_slice(
            &fs::read(barrier.join("input.request.event.json"))
                .expect("actual Core handler emitted the legal question event"),
        )
        .unwrap();
        // Keep the complete typed event and terminal in the receipt before semantic assertions.
        let outcomes = port
            .messages()
            .into_iter()
            .filter_map(|message| match message {
                wire::ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome),
                _ => None,
            })
            .collect::<Vec<_>>();
        let event_questions =
            find_event_field(&actual_event, "questions").expect("questions on the real Core event");
        let blocking = find_event_field(&actual_event, "isBlocking")
            .expect("blocking flag on real Core event");
        eprintln!(
            "MECHANISM_M14 {}",
            serde_json::json!({"mode":mode,"modeScope":if mode=="Plan"{"test injected existing Core mode; no production Plan API claim"}else{"production default mode"},"questionCount":count,"actualCoreEvent":actual_event,"outcomes":outcomes,"pollErrorCount":poll_error_count,"firstPollError":first_poll_error,"postFrameElapsedMillis":post_frame_started.elapsed().as_millis(),"inputRequests":port.messages().iter().filter(|message|matches!(message,wire::ExecutionPortMessage::InputRequestMessage(_))).count()})
        );
        assert_eq!(event_questions.as_array().unwrap().len(), count);
        assert_eq!(blocking.as_bool(), Some(mode == "Plan"));
        assert!(
            !outcomes.iter().any(|outcome| outcome.outcome.status
                == wire::ExecutionOutcomeStatus::InfrastructureError),
            "a legal Core event must not terminate its Job with InfrastructureError"
        );
        assert!(
            port.messages().iter().any(|message| matches!(
                message,
                wire::ExecutionPortMessage::InputRequestMessage(_)
            )),
            "legal questions must reach product input projection"
        );
        Box::pin(worker.shutdown(at("00:01:00"))).await.unwrap();
    });
}

fn find_event_field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    match value {
        serde_json::Value::Object(fields) => fields.get(key).or_else(|| {
            fields
                .values()
                .find_map(|value| find_event_field(value, key))
        }),
        serde_json::Value::Array(values) => {
            values.iter().find_map(|value| find_event_field(value, key))
        }
        _ => None,
    }
}

#[test]
#[ignore = "mechanism audit: known red; legal Default questions lack product projection"]
fn m14_default_two_questions_reach_product_without_infrastructure_terminal() {
    multiquestion_case(
        "Default",
        2,
        "mechanism_multiquestion::m14_default_two_questions_reach_product_without_infrastructure_terminal",
    );
}

#[test]
#[ignore = "mechanism audit: known red; test-injected Plan questions lack product projection"]
fn m14_plan_three_questions_reach_product_without_infrastructure_terminal() {
    multiquestion_case(
        "Plan",
        3,
        "mechanism_multiquestion::m14_plan_three_questions_reach_product_without_infrastructure_terminal",
    );
}
