// SPDX-License-Identifier: Apache-2.0

use crate::session::tests::make_session_and_context_with_rx;
use crate::state::ActiveTurn;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputResponse;
use pretty_assertions::assert_eq;
use std::collections::HashMap;

#[tokio::test]
async fn ra_c01_early_input_response_is_buffered_for_the_real_turn_waiter() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let response = RequestUserInputResponse {
        answers: HashMap::new(),
    };
    session
        .notify_user_input_response(&turn.sub_id, response.clone())
        .await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let request = tokio::spawn({
        let session = session.clone();
        let turn = turn.clone();
        async move {
            session
                .request_user_input(
                    turn.as_ref(),
                    "early-input".into(),
                    RequestUserInputArgs {
                        questions: Vec::new(),
                        is_blocking: true,
                        auto_resolution_ms: None,
                    },
                )
                .await
        }
    });
    assert!(matches!(
        events.recv().await.unwrap().msg,
        EventMsg::RequestUserInput(_)
    ));
    assert_eq!(request.await.unwrap(), Some(response));
}

#[tokio::test]
#[ignore = "mechanism audit: current behavior baseline; early ordinary approval is dropped"]
async fn ra_c01_early_approval_is_dropped_and_later_exact_waiter_stays_pending() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let decision = ReviewDecision::Denied {
        rejection: "synthetic ordinary denial".into(),
    };
    session
        .notify_approval("early-approval", decision.clone())
        .await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    #[allow(deprecated)]
    let cwd = turn.cwd.clone();
    let mut request = tokio::spawn({
        let session = session.clone();
        let turn = turn.clone();
        async move {
            session
                .request_command_approval(
                    turn.as_ref(),
                    "early-approval".into(),
                    /*approval_id*/ None,
                    /*environment_id*/ None,
                    vec!["echo".into()],
                    cwd,
                    /*reason*/ None,
                    /*network_approval_context*/ None,
                    /*proposed_execpolicy_amendment*/ None,
                    /*additional_permissions*/ None,
                    /*available_decisions*/ None,
                    /*plugin_attribution_override*/ None,
                )
                .await
        }
    });
    assert!(matches!(
        events.recv().await.unwrap().msg,
        EventMsg::ExecApprovalRequest(_)
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut request)
            .await
            .is_err(),
        "early ordinary approval was not buffered for its future waiter"
    );
    session
        .notify_approval("early-approval", decision.clone())
        .await;
    assert_eq!(request.await.unwrap(), decision);
}

#[tokio::test]
async fn ra_c01_early_input_response_is_buffered_in_the_active_turn_for_the_real_waiter() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let response = RequestUserInputResponse {
        answers: HashMap::from([(
            "continue".to_string(),
            codex_protocol::request_user_input::RequestUserInputAnswer {
                answers: vec!["continue".to_string()],
            },
        )]),
    };
    session
        .notify_user_input_response(&turn.sub_id, response.clone())
        .await;
    let request = tokio::spawn({
        let session = session.clone();
        let turn = turn.clone();
        async move {
            session
                .request_user_input(
                    turn.as_ref(),
                    "early-active-turn-input".into(),
                    RequestUserInputArgs {
                        questions: Vec::new(),
                        is_blocking: true,
                        auto_resolution_ms: None,
                    },
                )
                .await
        }
    });
    assert!(matches!(
        events.recv().await.unwrap().msg,
        EventMsg::RequestUserInput(_)
    ));
    assert_eq!(request.await.unwrap(), Some(response));
}
