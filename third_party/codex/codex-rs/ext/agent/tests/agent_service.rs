use anyhow::Result;
use codex_agent_extension::AgentInvocation;
use codex_agent_extension::AgentRunner;
use codex_protocol::protocol::EventMsg;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;

#[test]
fn starts_resolved_agent_prompt_in_forked_thread() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    let handle = std::thread::Builder::new()
        .name("agent-service-test".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(TEST_STACK_SIZE_BYTES)
                .enable_all()
                .build()
                .expect("agent service test runtime should build")
                .block_on(starts_resolved_agent_prompt_in_forked_thread_async())
        })
        .expect("agent service test thread should spawn");

    match handle.join() {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

async fn starts_resolved_agent_prompt_in_forked_thread_async() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("agent-response"),
            responses::ev_completed("agent-response"),
        ]),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let parent_thread_id = test.session_configured.session_id.into();
    let agent_runner = AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager));

    let agent_run = agent_runner
        .start(
            parent_thread_id,
            AgentInvocation {
                config: test.config.clone(),
                prompt: "Use $example-agent to inspect the current changes.".to_string(),
                parent_trace: None,
            },
        )
        .await?;

    assert_ne!(agent_run.thread_id, parent_thread_id);
    assert_eq!(
        agent_run
            .thread
            .config_snapshot()
            .await
            .forked_from_thread_id,
        Some(parent_thread_id)
    );
    let started = wait_for_event(&agent_run.thread, |event| {
        matches!(event, EventMsg::TurnStarted(_))
    })
    .await;
    let EventMsg::TurnStarted(started) = started else {
        unreachable!("event predicate only matches turn started events");
    };
    assert_eq!(started.turn_id, agent_run.turn_id);
    wait_for_event(&agent_run.thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let request = response_mock.single_request();
    assert!(
        request
            .message_input_texts("user")
            .iter()
            .any(|text| text == "Use $example-agent to inspect the current changes.")
    );

    Ok(())
}
