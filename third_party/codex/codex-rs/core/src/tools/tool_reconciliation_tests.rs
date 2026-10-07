// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::ToolCallGate;
use crate::ToolCallGateAuthorization;
use crate::ToolCallGateRejection;
use crate::ToolCallGateRequest;
use crate::session::step_context::StepContext;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use codex_state::ToolRecoveryEvidence;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

struct Gate(AtomicBool);
impl ToolCallGate for Gate {
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
        let allowed = self.0.load(Ordering::SeqCst);
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
        Box::pin(async { Ok(()) })
    }
}
struct ReceiptTool {
    effects: AtomicUsize,
    queries: AtomicUsize,
}
impl ToolExecutor<ToolInvocation> for ReceiptTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("receipt")
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "receipt".into(),
            description: "receipt test".into(),
            strict: false,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
        })
    }
    fn handle(&self, _: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(FunctionCallError::RespondToModel(
                "response lost after side effect".into(),
            ))
        })
    }
}
impl CoreToolRuntime for ReceiptTool {
    fn reconcile_execution<'a>(
        &'a self,
        original: &'a ToolInvocation,
    ) -> BoxFuture<'a, ToolRecoveryEvidence> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            ToolRecoveryEvidence::Running {
                business_id: original.call_id.clone(),
            }
        })
    }
}
#[tokio::test]
async fn unknown_effect_queries_original_receipt_without_retrying_or_changing_disposition() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let gate = Arc::new(Gate(AtomicBool::new(true)));
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
        call_id: "original-business-call".into(),
        tool_name: ToolName::plain("receipt").with_default_namespace(),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: "{}".into(),
        },
    };
    let tool = Arc::new(ReceiptTool {
        effects: AtomicUsize::new(0),
        queries: AtomicUsize::new(0),
    });
    let registry = ToolRegistry::with_handler_for_test(tool.clone());
    assert!(
        registry
            .dispatch_any_with_terminal_outcome(invocation.clone(), None)
            .await
            .is_err()
    );
    for _ in 0..2 {
        let error = registry
            .dispatch_any_with_terminal_outcome(invocation.clone(), None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("reconciled_execution_evidence"));
        assert!(error.contains("original-business-call"));
    }
    assert_eq!(
        (
            tool.effects.load(Ordering::SeqCst),
            tool.queries.load(Ordering::SeqCst)
        ),
        (1, 2)
    );
    let events =
        super::super::execution_facts::ExecutionFacts::read_events(&invocation.session, 0, 200)
            .await
            .unwrap();
    let reconciliations: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.fact {
            codex_state::ToolRuntimeFact::Reconciliation(fact) => Some(fact),
            _ => None,
        })
        .collect();
    assert_eq!(reconciliations.len(), 1);
    assert_eq!(
        reconciliations[0].attempt.execution,
        codex_state::ToolExecutionStatus::Uncertain
    );
    assert_eq!(
        reconciliations[0].attempt.disposition,
        codex_state::ToolOutputDisposition::Rejected
    );
    gate.0.store(false, Ordering::SeqCst);
    let error = registry
        .dispatch_any_with_terminal_outcome(invocation, None)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("result_read_denied"));
    assert_eq!(tool.queries.load(Ordering::SeqCst), 2);
}
