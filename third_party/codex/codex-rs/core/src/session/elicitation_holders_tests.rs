use std::collections::HashMap;

use codex_core_plugins::PluginCommandAttribution;
use codex_plugin::PluginId;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::request_permissions::PermissionGrantScope;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsArgs;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputResponse;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::tests::make_session_and_context_with_rx;
use crate::state::ActiveTurn;

async fn wait_until_held(pause_state: &mut watch::Receiver<bool>) {
    pause_state
        .wait_for(|paused| *paused)
        .await
        .expect("elicitation service should remain available");
}

async fn wait_until_released(pause_state: &mut watch::Receiver<bool>) {
    pause_state
        .wait_for(|paused| !*paused)
        .await
        .expect("elicitation service should remain available");
}

#[tokio::test]
async fn command_approval_holds_an_elicitation_until_response() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let mut pause_state = session.subscribe_elicitation_pause_state();
    #[allow(deprecated)]
    let cwd = turn_context.cwd.clone();
    let plugin_attribution = PluginCommandAttribution {
        plugin_id: PluginId::parse("sample@openai-curated").expect("valid plugin id"),
        normalized_relative_path: "scripts/run.py".to_string(),
    };

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_command_approval(
                    turn_context.as_ref(),
                    "call-1".to_string(),
                    /*approval_id*/ None,
                    /*environment_id*/ None,
                    vec!["echo".to_string()],
                    cwd,
                    /*reason*/ None,
                    /*network_approval_context*/ None,
                    /*proposed_execpolicy_amendment*/ None,
                    /*additional_permissions*/ None,
                    /*available_decisions*/ None,
                    /*plugin_attribution_override*/ Some(plugin_attribution),
                )
                .await
        }
    });

    let event = events.recv().await.expect("approval event");
    let codex_protocol::protocol::EventMsg::ExecApprovalRequest(event) = event.msg else {
        panic!("expected command approval event");
    };
    assert_eq!(event.plugin_id.as_deref(), Some("sample@openai-curated"));
    assert_eq!(event.script_path.as_deref(), Some("scripts/run.py"));
    wait_until_held(&mut pause_state).await;
    session
        .notify_approval("call-1", ReviewDecision::Approved)
        .await;
    request.await.expect("approval task");
    wait_until_released(&mut pause_state).await;
}

#[tokio::test]
async fn patch_approval_holds_an_elicitation_until_response() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let mut pause_state = session.subscribe_elicitation_pause_state();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_patch_approval(
                    turn_context.as_ref(),
                    "call-1".to_string(),
                    HashMap::new(),
                    /*reason*/ None,
                    /*grant_root*/ None,
                )
                .await
        }
    });

    events.recv().await.expect("approval event");
    wait_until_held(&mut pause_state).await;
    session
        .notify_approval("call-1", ReviewDecision::Approved)
        .await;
    request.await.expect("approval task");
    wait_until_released(&mut pause_state).await;
}

#[tokio::test]
async fn permission_request_holds_an_elicitation_until_response() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let mut pause_state = session.subscribe_elicitation_pause_state();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            let environment = turn_context
                .environments
                .primary()
                .expect("primary environment")
                .selection();
            session
                .request_permissions_for_environment(
                    &turn_context,
                    "call-1".to_string(),
                    RequestPermissionsArgs {
                        environment_id: None,
                        reason: None,
                        permissions: RequestPermissionProfile::default(),
                    },
                    environment,
                    CancellationToken::new(),
                )
                .await
        }
    });

    events.recv().await.expect("permission request event");
    wait_until_held(&mut pause_state).await;
    session
        .notify_request_permissions_response(
            "call-1",
            RequestPermissionsResponse {
                permissions: RequestPermissionProfile::default(),
                scope: PermissionGrantScope::Turn,
                strict_auto_review: false,
            },
        )
        .await;
    request.await.expect("permission request task");
    wait_until_released(&mut pause_state).await;
}

#[tokio::test]
async fn request_user_input_holds_an_elicitation_until_response() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let mut pause_state = session.subscribe_elicitation_pause_state();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_user_input(
                    turn_context.as_ref(),
                    "call-1".to_string(),
                    RequestUserInputArgs {
                        questions: Vec::new(),
                        is_blocking: true,
                        auto_resolution_ms: None,
                    },
                )
                .await
        }
    });

    events.recv().await.expect("request user input event");
    wait_until_held(&mut pause_state).await;

    let response = RequestUserInputResponse {
        answers: HashMap::new(),
    };
    session
        .notify_user_input_response(&turn_context.sub_id, "call-1", response)
        .await;

    request.await.expect("request user input task");
    wait_until_released(&mut pause_state).await;
}

#[tokio::test]
async fn unregistered_input_response_is_rejected_until_exact_restarted_waiter_exists() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let response = RequestUserInputResponse {
        answers: HashMap::new(),
    };

    assert!(
        !session
            .notify_user_input_response(&turn_context.sub_id, "call-restarted", response.clone())
            .await
    );
    *session.active_turn.lock().await = Some(ActiveTurn::default());

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_user_input(
                    turn_context.as_ref(),
                    "call-restarted".to_owned(),
                    RequestUserInputArgs {
                        questions: Vec::new(),
                        is_blocking: true,
                        auto_resolution_ms: None,
                    },
                )
                .await
        }
    });

    events
        .recv()
        .await
        .expect("restarted request user input event");
    tokio::task::yield_now().await;
    assert!(
        !request.is_finished(),
        "an unregistered old response cannot settle a future call"
    );
    assert!(
        session
            .notify_user_input_response(&turn_context.sub_id, "call-restarted", response.clone())
            .await
    );
    assert_eq!(
        request.await.expect("restarted request task"),
        Some(response)
    );
}

#[tokio::test]
async fn concurrent_same_turn_input_calls_preserve_each_original_waiter() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let spawn = |call_id: &'static str| {
        let session = session.clone();
        let turn_context = turn_context.clone();
        tokio::spawn(async move {
            session
                .request_user_input(
                    turn_context.as_ref(),
                    call_id.to_owned(),
                    RequestUserInputArgs {
                        questions: Vec::new(),
                        is_blocking: true,
                        auto_resolution_ms: None,
                    },
                )
                .await
        })
    };
    let first = spawn("parallel-input-1");
    events.recv().await.expect("first actual input event");
    let second = spawn("parallel-input-2");
    events.recv().await.expect("second actual input event");
    tokio::task::yield_now().await;
    assert!(
        !first.is_finished(),
        "second call must not cancel the first original waiter"
    );
    assert!(!second.is_finished());
    let response = |answer: &str| RequestUserInputResponse {
        answers: HashMap::from([(
            "same-question".into(),
            codex_protocol::request_user_input::RequestUserInputAnswer {
                answers: vec![answer.into()],
            },
        )]),
    };
    let second_response = response("second-answer");
    assert!(
        session
            .notify_user_input_response(
                &turn_context.sub_id,
                "parallel-input-2",
                second_response.clone()
            )
            .await
    );
    assert_eq!(
        second.await.expect("second input task"),
        Some(second_response)
    );
    assert!(
        !first.is_finished(),
        "reverse-order answer settles only its exact original waiter"
    );
    let first_response = response("first-answer");
    assert!(
        session
            .notify_user_input_response(
                &turn_context.sub_id,
                "parallel-input-1",
                first_response.clone()
            )
            .await
    );
    assert_eq!(first.await.expect("first input task"), Some(first_response));
}

#[tokio::test]
async fn late_cancelled_and_unknown_input_calls_cannot_answer_another_same_turn_waiter() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let spawn = |id: &'static str| {
        let session = session.clone();
        let turn = turn.clone();
        tokio::spawn(async move {
            session
                .request_user_input(
                    turn.as_ref(),
                    id.into(),
                    RequestUserInputArgs {
                        questions: Vec::new(),
                        is_blocking: true,
                        auto_resolution_ms: None,
                    },
                )
                .await
        })
    };
    let cancelled = spawn("cancelled-A");
    events.recv().await.unwrap();
    let response = RequestUserInputResponse {
        answers: HashMap::new(),
    };
    cancelled.abort();
    assert!(
        cancelled.await.unwrap_err().is_cancelled(),
        "original response receiver was actually cancelled"
    );
    let duplicate = spawn("cancelled-A");
    assert_eq!(
        duplicate.await.unwrap(),
        None,
        "same incarnation cannot reinstall a cancelled exact call"
    );
    let current = spawn("current-B");
    events.recv().await.unwrap();
    for id in ["cancelled-A", "unknown-call", ""] {
        assert!(
            !session
                .notify_user_input_response(&turn.sub_id, id, response.clone())
                .await
        );
    }
    assert_eq!(
        spawn("cancelled-A").await.unwrap(),
        None,
        "late answer must not erase the old-call tombstone"
    );
    tokio::task::yield_now().await;
    assert!(!current.is_finished());
    assert!(
        session
            .notify_user_input_response(&turn.sub_id, "current-B", response.clone())
            .await
    );
    assert_eq!(current.await.unwrap(), Some(response));
}
