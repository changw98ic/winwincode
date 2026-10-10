// SPDX-License-Identifier: Apache-2.0

use super::super::context::FunctionToolOutput;
use super::super::registry::ToolExecutor;
use super::super::registry::ToolRegistry;
use super::*;
use crate::session::step_context::StepContext;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

struct EffectTool(Arc<AtomicUsize>);
impl ToolExecutor<ToolInvocation> for EffectTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("effect")
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "effect".into(),
            description: "test effect".into(),
            strict: false,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
        })
    }
    fn handle(&self, _: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(
                Box::new(FunctionToolOutput::from_text("result".into(), Some(true)))
                    as Box<dyn super::super::context::ToolOutput>,
            )
        })
    }
}
impl CoreToolRuntime for EffectTool {}

async fn invocation() -> ToolInvocation {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let turn = Arc::new(turn);
    ToolInvocation {
        session: Arc::new(session),
        turn: turn.clone(),
        step_context: StepContext::for_test(turn),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "call-1".into(),
        tool_name: ToolName::plain("effect").with_default_namespace(),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: "{}".into(),
        },
    }
}

async fn begin_new(invocation: &ToolInvocation) -> Result<ToolFactRecord, FunctionCallError> {
    match ToolFactRecord::begin(invocation).await? {
        ToolFactAdmission::New(record) => Ok(record),
        ToolFactAdmission::Replay(replay) => {
            let active = replay
                .service
                .active_requests
                .lock()
                .unwrap()
                .contains(&replay.fact.request_sequence);
            Err(recovery_response(&replay.fact, active, &replay.owner_id))
        }
    }
}

fn recovery_state(error: FunctionCallError) -> serde_json::Value {
    let FunctionCallError::RespondToModel(message) = error else {
        panic!("recoverable response")
    };
    serde_json::from_str(&message).unwrap()
}

#[tokio::test]
async fn common_dispatch_records_execution_acceptance_and_offer_without_reexecuting_replays() {
    let invocation = invocation().await;
    let effects = Arc::new(AtomicUsize::new(0));
    let registry = ToolRegistry::from_tools([
        Arc::new(EffectTool(effects.clone())) as Arc<dyn CoreToolRuntime>
    ]);
    let result = registry
        .dispatch_any_with_terminal_outcome(invocation.clone(), None)
        .await
        .unwrap();
    assert_eq!(
        result.result.code_mode_result(&result.payload),
        serde_json::json!("result")
    );
    let replay = registry
        .dispatch_any_with_terminal_outcome(invocation.clone(), None)
        .await
        .err()
        .unwrap();
    assert_eq!(
        recovery_state(replay)["state"],
        "stored_result_requires_validation"
    );
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    let store = ExecutionFacts::for_session(&invocation.session)
        .store
        .get()
        .unwrap()
        .clone();
    let events = store
        .list_tool_fact_events(&invocation.session.thread_id.to_string(), 0, 200)
        .await
        .unwrap();
    assert_eq!(events.len(), 5);
    let receipt = events.last().unwrap().fact.attempt.as_ref().unwrap();
    assert_eq!(
        (receipt.execution, receipt.disposition, receipt.delivery),
        (
            ToolExecutionStatus::Completed,
            ToolOutputDisposition::Accepted,
            codex_state::ToolOutputDelivery::Offered
        )
    );
    let mut next = invocation;
    next.call_id = "new-logical-call".into();
    registry
        .dispatch_any_with_terminal_outcome(next, None)
        .await
        .unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn crash_before_dispatch_has_an_observation_without_an_attempt() {
    let invocation = invocation().await;
    begin_new(&invocation).await.unwrap();
    let error = begin_new(&invocation).await.err().unwrap();
    assert_eq!(recovery_state(error)["state"], "not_dispatched");
}

#[tokio::test]
async fn unknown_effect_or_interrupted_output_hook_never_reexecutes() {
    for completed in [false, true] {
        let invocation = invocation().await;
        let record = begin_new(&invocation).await.unwrap();
        record
            .claim(&invocation, &EffectTool(Arc::new(AtomicUsize::new(0))))
            .await
            .unwrap();
        if completed {
            record
                .completed(Some("recorded-before-post-hook".into()))
                .await
                .unwrap();
        }
        drop(record);
        let replay = begin_new(&invocation).await.err().unwrap();
        assert_eq!(
            recovery_state(replay)["state"],
            if completed {
                "output_decision_pending"
            } else {
                "reconciliation_required"
            }
        );
    }
}

#[tokio::test]
async fn rejection_and_changed_input_have_distinct_recovery_outcomes() {
    let invocation = invocation().await;
    let record = begin_new(&invocation).await.unwrap();
    record.deny().await.unwrap();
    let replay = begin_new(&invocation).await.err().unwrap();
    assert_eq!(recovery_state(replay)["state"], "request_denied");
    let mut changed = invocation;
    changed.payload = ToolPayload::Function {
        arguments: "{\"changed\":true}".into(),
    };
    let error = begin_new(&changed).await.err().unwrap();
    assert!(error.to_string().contains("tool_request_identity_conflict"));
}

#[tokio::test]
async fn nested_identity_and_parent_survive_a_new_turn_context() {
    let mut invocation = invocation().await;
    ExecutionFacts::for_session(&invocation.session).register_cell(
        "cell".into(),
        "outer-call".into(),
        "original-turn",
    );
    invocation.source = ToolCallSource::CodeMode {
        cell_id: "cell".into(),
        runtime_tool_call_id: "0".into(),
    };
    let record = begin_new(&invocation).await.unwrap();
    let (_, mut new_turn) = crate::session::tests::make_session_and_context().await;
    new_turn.sub_id = "next-turn".into();
    invocation.turn = Arc::new(new_turn);
    invocation.step_context = StepContext::for_test(invocation.turn.clone());
    let replay = begin_new(&invocation).await.err().unwrap();
    assert_eq!(recovery_state(replay)["request_sequence"], record.sequence);
    let events = record
        .store
        .list_tool_fact_events(&invocation.session.thread_id.to_string(), 0, 200)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].fact.request.parent_call_id,
        Some("outer-call".into())
    );
}

#[tokio::test]
async fn a_new_cell_owner_does_not_collide_with_historical_cell_numbers() {
    let mut invocation = invocation().await;
    let facts = ExecutionFacts::for_session(&invocation.session);
    invocation.source = ToolCallSource::CodeMode {
        cell_id: "cell-1".into(),
        runtime_tool_call_id: "0".into(),
    };
    facts.register_cell("cell-1".into(), "first-exec".into(), "first-turn");
    let first = begin_new(&invocation).await.unwrap();
    facts.register_cell("cell-1".into(), "second-exec".into(), "second-turn");
    let second = begin_new(&invocation).await.unwrap();
    assert_ne!(first.sequence, second.sequence);
}

#[tokio::test]
async fn code_mode_direct_admission_rejection_is_an_observation() {
    let mut invocation = invocation().await;
    invocation.turn = Arc::new({
        let (_, mut turn) = crate::session::tests::make_session_and_context().await;
        Arc::make_mut(&mut turn.model_info).tool_mode =
            Some(codex_protocol::openai_models::ToolMode::CodeModeOnly);
        turn
    });
    invocation.step_context = StepContext::for_test(invocation.turn.clone());
    let effects = Arc::new(AtomicUsize::new(0));
    let registry = ToolRegistry::from_tools([
        Arc::new(EffectTool(effects.clone())) as Arc<dyn CoreToolRuntime>
    ]);
    assert!(
        registry
            .dispatch_any_with_terminal_outcome(invocation.clone(), None)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("must be invoked through Code Mode")
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let facts = ExecutionFacts::for_session(&invocation.session);
    let events = facts
        .store
        .get()
        .unwrap()
        .list_tool_fact_events(&invocation.session.thread_id.to_string(), 0, 200)
        .await
        .unwrap();
    assert_eq!(
        events.last().unwrap().fact.resolution,
        ToolRequestResolution::Denied
    );
}

#[tokio::test]
async fn active_attempt_is_distinct_from_lost_execution_owner() {
    let invocation = invocation().await;
    let record = begin_new(&invocation).await.unwrap();
    record
        .claim(&invocation, &EffectTool(Arc::new(AtomicUsize::new(0))))
        .await
        .unwrap();
    let replay = begin_new(&invocation).await.err().unwrap();
    assert_eq!(recovery_state(replay)["state"], "in_flight");
    drop(record);
    let replay = begin_new(&invocation).await.err().unwrap();
    assert_eq!(recovery_state(replay)["state"], "reconciliation_required");
}

#[tokio::test]
async fn another_core_incarnation_does_not_assert_the_previous_owner_is_dead() {
    let first = invocation().await;
    let record = begin_new(&first).await.unwrap();
    record
        .claim(&first, &EffectTool(Arc::new(AtomicUsize::new(0))))
        .await
        .unwrap();
    let mut second = invocation().await;
    Arc::get_mut(&mut second.session).unwrap().thread_id = first.session.thread_id;
    let (_, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.sub_id = first.turn.sub_id.clone();
    second.turn = Arc::new(turn);
    second.step_context = StepContext::for_test(second.turn.clone());
    let facts = ExecutionFacts::for_session(&second.session);
    assert!(facts.store.set(record.store.clone()).is_ok());
    let replay = begin_new(&second).await.err().unwrap();
    assert_eq!(
        recovery_state(replay)["state"],
        "execution_owner_unverified"
    );
}

struct UnreturnedEffect {
    inner: EffectTool,
    started: Arc<tokio::sync::Notify>,
}
impl ToolExecutor<ToolInvocation> for UnreturnedEffect {
    fn tool_name(&self) -> ToolName {
        self.inner.tool_name()
    }
    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }
    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        let output = self.inner.handle(invocation);
        Box::pin(async move {
            let output = output.await?;
            self.started.notify_one();
            std::future::pending::<()>().await;
            Ok(output)
        })
    }
}
impl CoreToolRuntime for UnreturnedEffect {}

#[tokio::test]
async fn interrupted_handler_after_side_effect_requires_reconciliation_without_reexecution() {
    let invocation = invocation().await;
    let effects = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let registry = Arc::new(ToolRegistry::from_tools([Arc::new(UnreturnedEffect {
        inner: EffectTool(effects.clone()),
        started: started.clone(),
    })
        as Arc<dyn CoreToolRuntime>]));
    let dispatch = tokio::spawn({
        let registry = registry.clone();
        let invocation = invocation.clone();
        async move {
            registry
                .dispatch_any_with_terminal_outcome(invocation, None)
                .await
        }
    });
    started.notified().await;
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    dispatch.abort();
    assert!(dispatch.await.err().unwrap().is_cancelled());
    let replay = registry
        .dispatch_any_with_terminal_outcome(invocation, None)
        .await
        .err()
        .unwrap();
    assert_eq!(recovery_state(replay)["state"], "reconciliation_required");
    assert_eq!(effects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn restarted_wait_reports_historical_cell_owner_without_creating_a_wait_edge() {
    let first = invocation().await;
    let record = begin_new(&first).await.unwrap();
    record
        .claim(&first, &EffectTool(Arc::new(AtomicUsize::new(0))))
        .await
        .unwrap();
    ExecutionFacts::start_cell(
        &first.session,
        &first.turn,
        "historical-cell",
        &first.call_id,
    )
    .await
    .unwrap();
    let mut second = invocation().await;
    Arc::get_mut(&mut second.session).unwrap().thread_id = first.session.thread_id;
    let facts = ExecutionFacts::for_session(&second.session);
    assert!(facts.store.set(record.store.clone()).is_ok());
    let error = ExecutionFacts::begin_cell_wait(
        &second.session,
        &second.turn,
        "historical-cell",
        &second.call_id,
        1000,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(recovery_state(error)["state"], "cell_owner_unverified");
    let events = record
        .store
        .list_tool_runtime_events(&first.session.thread_id.to_string(), 0, 200)
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event.fact, codex_state::ToolRuntimeFact::Wait(_)))
    );
}
