//! Verifies that the agent retries when the SSE stream terminates before
//! delivering a `response.completed` event.

use codex_core::TurnInputRequest;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::net::TcpListener;
use wiremock::MockServer;

fn sse_incomplete() -> String {
    responses::sse(vec![serde_json::json!({
        "type": "response.output_item.done",
    })])
}

#[test_case::test_case(false, false; "legacy_completed")]
#[test_case::test_case(true, false; "code_mode_completed")]
#[test_case::test_case(false, true; "legacy_running")]
#[test_case::test_case(true, true; "code_mode_running")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_close_retains_started_tool_output(
    code_mode: bool,
    running: bool,
) -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("effects");
    let release = directory.path().join("release");
    let command = format!(
        "{}printf ran >> '{}'; printf retained-%s result",
        if running {
            format!(
                "while [ ! -f '{}' ]; do sleep 0.01; done; ",
                release.display()
            )
        } else {
            String::new()
        },
        marker.display()
    );
    let call = if code_mode {
        responses::ev_code_mode_call(
            "call-retained",
            "functions",
            "exec_command",
            &serde_json::json!({"cmd":command,"login":false}).to_string(),
        )
    } else {
        responses::ev_shell_command_call_with_args(
            "call-retained",
            &serde_json::json!({"command":command,"login":false}),
        )
    };
    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("resp-cut"), call]),
            },
            StreamingSseChunk {
                gate: Some(close_rx),
                body: String::new(),
            },
        ],
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse_completed("resp-retry"),
        }],
    ])
    .await;
    let test = test_codex()
        .with_model("gpt-5.4")
        .with_config(move |config| {
            config.model_provider.stream_max_retries = Some(1);
            config.model_provider.supports_websockets = false;
            if code_mode {
                let _ = config
                    .features
                    .enable(codex_features::Feature::CodeModeOnly);
                config.web_search_mode = codex_config::Constrained::allow_any(
                    codex_protocol::config_types::WebSearchMode::Disabled,
                );
            }
        })
        .build_with_streaming_server(&server)
        .await?;
    let codex = &test.codex;
    let (sandbox_policy, permission_profile) =
        core_test_support::test_codex::turn_permission_fields(
            codex_protocol::models::PermissionProfile::Disabled,
            test.cwd.path(),
        );
    codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "run the tool then recover the stream".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(
                codex_protocol::protocol::ThreadSettingsOverrides {
                    environments: Some(core_test_support::test_codex::local_selections(
                        test.config.cwd.clone(),
                    )),
                    approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                    sandbox_policy: Some(sandbox_policy),
                    permission_profile,
                    ..Default::default()
                },
            ),
        )
        .await?;
    let mut observed = Vec::new();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), codex.next_event())
            .await
            .unwrap_or_else(|_| panic!("waiting for tool completion: {observed:?}"))?;
        if (running && matches!(event.msg, EventMsg::ExecCommandBegin(_)))
            || (!running && matches!(event.msg, EventMsg::ExecCommandEnd(_)))
        {
            break;
        }
        observed.push(event.msg);
    }
    let _ = close_tx.send(());
    if running {
        std::fs::write(release, b"release")?;
    }
    wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    let retry: serde_json::Value = serde_json::from_slice(&requests[1])?;
    let outputs: Vec<_> = retry["input"]
        .as_array()
        .expect("retry input must be an array")
        .iter()
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call_output" | "custom_tool_call_output")
            )
        })
        .collect();
    assert!(
        outputs
            .iter()
            .any(|item| item.to_string().contains("retained-result")),
        "retry must contain the actual completed output: {outputs:?}"
    );
    assert_eq!(std::fs::read_to_string(marker)?, "ran");
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_on_early_close() {
    skip_if_no_network!();

    let incomplete_sse = sse_incomplete();
    let completed_sse = responses::sse_completed("resp_ok");

    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: incomplete_sse,
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: completed_sse,
        }],
    ])
    .await;

    // Configure retry behavior explicitly to avoid mutating process-wide
    // environment variables.

    let model_provider = ModelProviderInfo {
        name: "openai".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        // Environment variable that should exist in the test environment.
        // ModelClient will return an error if the environment variable for the
        // provider is not set.
        env_key: Some("PATH".into()),
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        // exercise retry path: first attempt yields incomplete stream, so allow 1 retry
        request_max_retries: Some(0),
        stream_max_retries: Some(1),
        stream_idle_timeout_ms: Some(2000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
    };

    let TestCodex { codex, .. } = test_codex()
        .with_config(move |config| {
            config.model_provider = model_provider;
        })
        .build_with_streaming_server(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    // Wait until TurnComplete (should succeed after retry).
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = server.requests().await;
    assert_eq!(
        requests.len(),
        2,
        "expected retry after incomplete SSE stream"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_failure_pauses_retry_budget_until_provider_is_reachable() -> anyhow::Result<()>
{
    skip_if_no_network!(Ok(()));

    let bootstrap_server = responses::start_mock_server().await;
    let unavailable_listener = TcpListener::bind("127.0.0.1:0")?;
    let unavailable_address = unavailable_listener.local_addr()?;
    drop(unavailable_listener);

    let TestCodex { codex, .. } = test_codex()
        .with_config(move |config| {
            config.model_provider.base_url = Some(format!("http://{unavailable_address}/v1"));
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&bootstrap_server)
        .await?;

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "recover after the network returns".into(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let EventMsg::StreamError(connection_error) =
        wait_for_event(&codex, |event| matches!(event, EventMsg::StreamError(_))).await
    else {
        unreachable!("predicate guarantees a stream error event");
    };
    assert_eq!(
        connection_error.message,
        "Reconnecting... waiting for network"
    );

    let recovered_server = MockServer::builder()
        .listener(TcpListener::bind(unavailable_address)?)
        .start()
        .await;
    let response_mock = responses::mount_sse_sequence(
        &recovered_server,
        vec![sse_incomplete(), responses::sse_completed("resp_recovered")],
    )
    .await;

    let EventMsg::StreamError(stream_error) =
        wait_for_event(&codex, |event| matches!(event, EventMsg::StreamError(_))).await
    else {
        unreachable!("predicate guarantees a stream error event");
    };
    assert_eq!(stream_error.message, "Reconnecting... 1/1");

    let EventMsg::TurnComplete(completed) =
        wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await
    else {
        unreachable!("predicate guarantees a turn complete event");
    };

    assert_eq!(completed.error, None);
    assert_eq!(response_mock.requests().len(), 2);

    Ok(())
}
