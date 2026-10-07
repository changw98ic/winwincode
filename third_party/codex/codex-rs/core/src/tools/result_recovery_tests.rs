// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::ToolCallGate;
use crate::ToolCallGateAuthorization;
use crate::ToolCallGateRejection;
use crate::ToolCallGateRequest;
use crate::session::step_context::StepContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

struct ReadGate {
    allowed: AtomicBool,
    revoke_on_read: AtomicBool,
    reads: AtomicUsize,
}
impl ToolCallGate for ReadGate {
    fn authorize(
        &self,
        _: ToolCallGateRequest,
    ) -> BoxFuture<'static, Result<ToolCallGateAuthorization, ToolCallGateRejection>> {
        Box::pin(async { Ok(ToolCallGateAuthorization::default()) })
    }
    fn revalidate(
        &self,
        _: ToolCallGateRequest,
        _: ToolCallGateAuthorization,
    ) -> BoxFuture<'static, Result<(), ToolCallGateRejection>> {
        Box::pin(async { Ok(()) })
    }
    fn authorize_result_read(
        &self,
        _: ToolResultReadRequest,
    ) -> BoxFuture<'static, Result<ToolCallGateAuthorization, ToolCallGateRejection>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let allowed = self.allowed.load(Ordering::SeqCst);
        if self.revoke_on_read.load(Ordering::SeqCst) {
            self.allowed.store(false, Ordering::SeqCst);
        }
        Box::pin(async move {
            if allowed {
                Ok(ToolCallGateAuthorization::default())
            } else {
                Err(ToolCallGateRejection::new("denied", "denied"))
            }
        })
    }
    fn revalidate_result_read(
        &self,
        _: ToolResultReadRequest,
        _: ToolCallGateAuthorization,
    ) -> BoxFuture<'static, Result<(), ToolCallGateRejection>> {
        let allowed = self.allowed.load(Ordering::SeqCst);
        Box::pin(async move {
            if allowed {
                Ok(())
            } else {
                Err(ToolCallGateRejection::new("stale", "stale"))
            }
        })
    }
}
struct HistoryTool {
    effects: Arc<AtomicUsize>,
    accepted: Arc<AtomicUsize>,
    description: &'static str,
}
impl ToolExecutor<ToolInvocation> for HistoryTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("history")
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "history".into(),
            description: self.description.into(),
            strict: false,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
        })
    }
    fn handle(&self, _: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(Box::new(FunctionToolOutput::from_text(
                "private historical result".into(),
                Some(true),
            )) as Box<dyn ToolOutput>)
        })
    }
}
impl CoreToolRuntime for HistoryTool {
    fn supports_result_replay(&self) -> bool {
        true
    }
    fn on_tool_result_accepted(&self, _: &ToolInvocation, _: &dyn ToolOutput) {
        self.accepted.fetch_add(1, Ordering::SeqCst);
    }
}
async fn setup() -> (ToolInvocation, Arc<ReadGate>, Arc<HistoryTool>) {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let gate = Arc::new(ReadGate {
        allowed: AtomicBool::new(true),
        revoke_on_read: AtomicBool::new(false),
        reads: AtomicUsize::new(0),
    });
    session
        .services
        .thread_extension_data
        .insert(ToolCallGateAttachment::new(gate.clone()));
    let turn = Arc::new(turn);
    let invocation = ToolInvocation {
        session: Arc::new(session),
        turn: turn.clone(),
        step_context: StepContext::for_test(turn),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "history-call".into(),
        tool_name: ToolName::plain("history").with_default_namespace(),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: "{\"original\":true}".into(),
        },
    };
    let tool = Arc::new(HistoryTool {
        effects: Arc::new(AtomicUsize::new(0)),
        accepted: Arc::new(AtomicUsize::new(0)),
        description: "first version",
    });
    (invocation, gate, tool)
}
#[tokio::test]
async fn accepted_replay_restores_output_without_handler_or_acceptance_callback() {
    let (invocation, gate, tool) = setup().await;
    let registry = ToolRegistry::with_handler_for_test(tool.clone());
    let first = registry
        .dispatch_any_with_terminal_outcome(invocation.clone(), None)
        .await
        .unwrap()
        .into_response();
    let replay = registry
        .dispatch_any_with_terminal_outcome(invocation, None)
        .await
        .unwrap()
        .into_response();
    assert_eq!(first, replay);
    assert_eq!(
        (
            tool.effects.load(Ordering::SeqCst),
            tool.accepted.load(Ordering::SeqCst),
            gate.reads.load(Ordering::SeqCst)
        ),
        (1, 1, 1)
    );
}
#[tokio::test]
async fn read_denial_and_revocation_do_not_leak_body_or_repeat_effect() {
    for revoke_during_read in [false, true] {
        let (invocation, gate, tool) = setup().await;
        let registry = ToolRegistry::with_handler_for_test(tool.clone());
        registry
            .dispatch_any_with_terminal_outcome(invocation.clone(), None)
            .await
            .unwrap();
        gate.allowed.store(revoke_during_read, Ordering::SeqCst);
        gate.revoke_on_read
            .store(revoke_during_read, Ordering::SeqCst);
        let error = registry
            .dispatch_any_with_terminal_outcome(invocation, None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("result_read_denied"));
        assert!(!error.contains("private historical result"));
        assert_eq!(tool.effects.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn changed_registered_definition_expires_scope_without_reexecution() {
    let (invocation, _, tool) = setup().await;
    ToolRegistry::with_handler_for_test(tool.clone())
        .dispatch_any_with_terminal_outcome(invocation.clone(), None)
        .await
        .unwrap();
    let changed = Arc::new(HistoryTool {
        effects: tool.effects.clone(),
        accepted: tool.accepted.clone(),
        description: "second version",
    });
    let error = ToolRegistry::with_handler_for_test(changed)
        .dispatch_any_with_terminal_outcome(invocation, None)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("stored_result_scope_expired"));
    assert_eq!(tool.effects.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn restarted_core_recovers_accepted_receipt_under_current_read_authority() {
    let (first, _, tool) = setup().await;
    let registry = ToolRegistry::with_handler_for_test(tool.clone());
    let expected = registry
        .dispatch_any_with_terminal_outcome(first.clone(), None)
        .await
        .unwrap()
        .into_response();
    let (mut session, _) = crate::session::tests::make_session_and_context().await;
    session.thread_id = first.session.thread_id;
    let gate = Arc::new(ReadGate {
        allowed: AtomicBool::new(true),
        revoke_on_read: AtomicBool::new(false),
        reads: AtomicUsize::new(0),
    });
    session
        .services
        .thread_extension_data
        .insert(ToolCallGateAttachment::new(gate));
    let restarted = ToolInvocation {
        session: Arc::new(session),
        ..first
    };
    let actual = registry
        .dispatch_any_with_terminal_outcome(restarted, None)
        .await
        .unwrap()
        .into_response();
    assert_eq!(actual, expected);
    assert_eq!(tool.effects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn switching_to_code_mode_only_keeps_direct_replay_outside_the_model_surface() {
    let (mut invocation, gate, tool) = setup().await;
    let registry = ToolRegistry::with_handler_for_test(tool.clone());
    registry
        .dispatch_any_with_terminal_outcome(invocation.clone(), None)
        .await
        .unwrap();
    let (_, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.sub_id = invocation.turn.sub_id.clone();
    Arc::make_mut(&mut turn.model_info).tool_mode =
        Some(codex_protocol::openai_models::ToolMode::CodeModeOnly);
    invocation.turn = Arc::new(turn);
    invocation.step_context = StepContext::for_test(invocation.turn.clone());
    let error = registry
        .dispatch_any_with_terminal_outcome(invocation, None)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("must be invoked through Code Mode"));
    assert_eq!(gate.reads.load(Ordering::SeqCst), 0);
    assert_eq!(tool.effects.load(Ordering::SeqCst), 1);
}
