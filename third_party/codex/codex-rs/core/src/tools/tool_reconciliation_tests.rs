// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::ToolCallGate;
use crate::ToolCallGateAuthorization;
use crate::ToolCallGateRejection;
use crate::ToolCallGateRequest;
use crate::session::step_context::StepContext;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolPayload;
use crate::tools::execution_facts::ToolFactAdmission;
use crate::tools::execution_facts::ToolFactRecord;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use codex_state::StateRuntime;
use codex_state::ToolExecutionFact;
use codex_state::ToolRecoveryEvidence;
use codex_state::ToolRuntimeFact;
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
    evidence: Option<ToolRecoveryEvidence>,
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
        let evidence = self.evidence.clone();
        Box::pin(async move {
            evidence.unwrap_or_else(|| ToolRecoveryEvidence::Running {
                business_id: original.call_id.clone(),
            })
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
        evidence: None,
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

struct PublicationGate {
    store: Arc<StateRuntime>,
    original: ToolExecutionFact,
    request: ToolResultReadRequest,
    binding: String,
    revoke_after_publication: bool,
    read_checks: Arc<AtomicUsize>,
    publication_checks: Arc<AtomicUsize>,
}

impl ToolCallGate for PublicationGate {
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
        request: ToolResultReadRequest,
    ) -> BoxFuture<'static, Result<ToolCallGateAuthorization, ToolCallGateRejection>> {
        let result = if request == self.request {
            Ok(ToolCallGateAuthorization::new(self.binding.clone(), None))
        } else {
            Err(ToolCallGateRejection::new("denied", "receipt differs"))
        };
        Box::pin(async move { result })
    }

    fn revalidate_result_read(
        &self,
        request: ToolResultReadRequest,
        authorization: ToolCallGateAuthorization,
    ) -> BoxFuture<'static, Result<(), ToolCallGateRejection>> {
        let exact = request == self.request && authorization.request_binding() == self.binding;
        let store = Arc::clone(&self.store);
        let original = self.original.clone();
        let read_checks = Arc::clone(&self.read_checks);
        let publication_checks = Arc::clone(&self.publication_checks);
        let revoke_after_publication = self.revoke_after_publication;
        Box::pin(async move {
            if !exact {
                return Err(ToolCallGateRejection::new("denied", "receipt differs"));
            }
            let current = store
                .tool_execution_fact(original.request_sequence)
                .await
                .unwrap();
            assert_eq!(current, original);
            let events = store
                .list_tool_runtime_events(&original.request.thread_id, 0, 200)
                .await
                .unwrap();
            let published: Vec<_> = events
                .iter()
                .filter_map(|event| match &event.fact {
                    ToolRuntimeFact::Reconciliation(fact) => Some(fact),
                    _ => None,
                })
                .collect();
            let check = read_checks.fetch_add(1, Ordering::SeqCst);
            if check == 0 {
                assert!(
                    published.is_empty(),
                    "first read check precedes actual State publication"
                );
                return Ok(());
            }
            assert_eq!(
                published.len(),
                1,
                "final read check observes the committed State fact"
            );
            assert_eq!(published[0].request_sequence, original.request_sequence);
            assert_eq!(Some(&published[0].attempt), original.attempt.as_ref());
            publication_checks.fetch_add(1, Ordering::SeqCst);
            if revoke_after_publication {
                Err(ToolCallGateRejection::new(
                    "denied",
                    "read revoked after State publication",
                ))
            } else {
                Ok(())
            }
        })
    }
}

async fn assert_publication_result_read(revoke_after_publication: bool) {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    session
        .services
        .thread_extension_data
        .insert(ToolCallGateAttachment::new(Arc::new(Gate(
            AtomicBool::new(true),
        ))));
    let turn = Arc::new(turn);
    let invocation = ToolInvocation {
        session: Arc::new(session),
        turn: Arc::clone(&turn),
        step_context: StepContext::for_test(turn),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "publication-original-call".into(),
        tool_name: ToolName::plain("receipt").with_default_namespace(),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: "{}".into(),
        },
    };
    // This synthetic typed evidence exercises delivery of a private reference;
    // it does not simulate consumption of a user-input waiter.
    let evidence = ToolRecoveryEvidence::ControlInputResolved {
        input_request_sequence: 41,
        input_operation_digest: "synthetic-input-operation".into(),
        response_digest: "synthetic-response-digest".into(),
        result_ref: "publication-private-result-ref".into(),
        continuation_lost: true,
    };
    let tool = Arc::new(ReceiptTool {
        effects: AtomicUsize::new(0),
        queries: AtomicUsize::new(0),
        evidence: Some(evidence.clone()),
    });
    let registry = ToolRegistry::with_handler_for_test(tool.clone());
    assert!(
        registry
            .dispatch_any_with_terminal_outcome(invocation.clone(), None)
            .await
            .is_err()
    );
    let ToolFactAdmission::Replay(replay) = ToolFactRecord::begin(&invocation).await.unwrap()
    else {
        panic!("the original uncertain operation is a replay");
    };
    let store = Arc::clone(&replay.store);
    let original = (*replay.fact).clone();
    let attempt = original.attempt.as_ref().unwrap();
    assert_eq!(
        attempt.execution,
        codex_state::ToolExecutionStatus::Uncertain
    );
    assert_eq!(
        attempt.disposition,
        codex_state::ToolOutputDisposition::Rejected
    );
    let request = ToolResultReadRequest {
        thread_id: invocation.session.thread_id.to_string(),
        turn_id: invocation.turn.sub_id.clone(),
        call_id: invocation.call_id.clone(),
        request_sequence: original.request_sequence,
        logical_id: original.request.logical_id.clone(),
        attempt_id: attempt.attempt_id.clone(),
        operation_digest: attempt.operation_digest.clone(),
        revision: attempt.revision,
    };
    drop(replay);
    let binding = digest(serde_json::to_value(&request).unwrap());
    let gate = Arc::new(PublicationGate {
        store: Arc::clone(&store),
        original: original.clone(),
        request,
        binding,
        revoke_after_publication,
        read_checks: Arc::new(AtomicUsize::new(0)),
        publication_checks: Arc::new(AtomicUsize::new(0)),
    });
    invocation
        .session
        .services
        .thread_extension_data
        .insert(ToolCallGateAttachment::new(gate.clone()));
    let error = registry
        .dispatch_any_with_terminal_outcome(invocation, None)
        .await
        .err()
        .unwrap()
        .to_string();
    assert_eq!(
        (
            tool.effects.load(Ordering::SeqCst),
            tool.queries.load(Ordering::SeqCst)
        ),
        (1, 1)
    );
    assert_eq!(
        store
            .tool_execution_fact(original.request_sequence)
            .await
            .unwrap(),
        original
    );
    let events = store
        .list_tool_runtime_events(&original.request.thread_id, 0, 200)
        .await
        .unwrap();
    let published: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.fact {
            ToolRuntimeFact::Reconciliation(fact) => Some(fact),
            _ => None,
        })
        .collect();
    assert_eq!(published.len(), 1);
    assert_eq!(
        published[0].evidence, evidence,
        "private history remains committed"
    );
    if revoke_after_publication {
        assert!(
            error.contains("result_read_denied"),
            "published fact crossed revoked read authority: {error}"
        );
        assert!(!error.contains("reconciled_execution_evidence"));
        assert!(!error.contains("publication-private-result-ref"));
    } else {
        assert!(error.contains("reconciled_execution_evidence"));
        assert!(error.contains("publication-private-result-ref"));
    }
    assert_eq!(gate.read_checks.load(Ordering::SeqCst), 2);
    assert_eq!(gate.publication_checks.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn result_read_revoked_after_actual_state_publication_withholds_fact_and_private_reference() {
    assert_publication_result_read(true).await;
}

#[tokio::test]
async fn result_read_current_after_actual_state_publication_delivers_fact_without_retrying() {
    assert_publication_result_read(false).await;
}
