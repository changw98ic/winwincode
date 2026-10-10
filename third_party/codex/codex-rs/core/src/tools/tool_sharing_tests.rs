// SPDX-License-Identifier: Apache-2.0
use super::super::context::FunctionToolOutput;
use super::super::context::ToolCallSource;
use super::super::execution_facts::ExecutionFacts;
use super::super::execution_facts::ToolFactAdmission;
use super::super::registry::ToolExecutor;
use super::super::registry::ToolRegistry;
use super::*;
use crate::ToolCallGateAuthorization;
use crate::ToolCallGateRejection;
use crate::session::step_context::StepContext;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;

struct Gate {
    epoch: AtomicUsize,
    reads: AtomicBool,
    reuse: bool,
    merge: bool,
}
impl ToolCallGate for Gate {
    fn freeze_tool_input(
        &self,
        _: ToolInputContext,
    ) -> BoxFuture<'static, Option<ToolDependencySnapshot>> {
        let epoch = self.epoch.load(Ordering::SeqCst);
        let snapshot = ToolDependencySnapshot {
            policy_revision: "1".repeat(64),
            dependency_digest: "a".repeat(64),
            account_scope_digest: format!("{epoch:064x}"),
            session_scope_digest: "3".repeat(64),
            validity_epoch: "4".repeat(64),
            reuse: if self.reuse {
                crate::ToolReusePermission::ImmutableValue
            } else {
                crate::ToolReusePermission::Denied
            },
            coalescing: if self.merge {
                ToolCoalescingPermission::SharedRead
            } else {
                ToolCoalescingPermission::Denied
            },
        };
        Box::pin(async move { Some(snapshot) })
    }
    fn verify_tool_input(
        &self,
        _: ToolInputProofRequest,
    ) -> BoxFuture<'static, Option<crate::ToolInputProof>> {
        Box::pin(async {
            Some(crate::ToolInputProof {
                input_digest: "a".repeat(64),
                evidence_digest: "e".repeat(64),
            })
        })
    }
    fn authorize_result_read(
        &self,
        _: ToolResultReadRequest,
    ) -> BoxFuture<'static, Result<ToolCallGateAuthorization, ToolCallGateRejection>> {
        let permitted = self.reads.load(Ordering::SeqCst);
        Box::pin(async move {
            if permitted {
                Ok(ToolCallGateAuthorization::default())
            } else {
                Err(ToolCallGateRejection::new("DENIED", "read denied"))
            }
        })
    }
    fn revalidate_result_read(
        &self,
        request: ToolResultReadRequest,
        _: ToolCallGateAuthorization,
    ) -> BoxFuture<'static, Result<(), ToolCallGateRejection>> {
        let read = self.authorize_result_read(request);
        Box::pin(async move { read.await.map(|_| ()) })
    }
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
}
struct ReadTool {
    calls: AtomicUsize,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    wait: bool,
}
impl ToolExecutor<ToolInvocation> for ReadTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("trusted_read")
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "trusted_read".into(),
            description: "trusted test read".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::default(),
            output_schema: None,
        })
    }
    fn handle(&self, _: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            self.started.notify_one();
            if self.wait {
                self.release.notified().await;
            }
            Ok(Box::new(FunctionToolOutput::from_text(
                "verified output".into(),
                Some(true),
            )) as Box<dyn super::super::context::ToolOutput>)
        })
    }
}
impl CoreToolRuntime for ReadTool {}
async fn invocation(gate: Arc<Gate>) -> ToolInvocation {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    session
        .services
        .thread_extension_data
        .insert(ToolCallGateAttachment::new(gate));
    let turn = Arc::new(turn);
    ToolInvocation {
        session: Arc::new(session),
        turn: Arc::clone(&turn),
        step_context: StepContext::for_test(turn),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(crate::turn_diff_tracker::TurnDiffTracker::new())),
        call_id: "first".into(),
        tool_name: ToolName::plain("trusted_read").with_default_namespace(),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: "{}".into(),
        },
    }
}
fn gate(reuse: bool, merge: bool) -> Arc<Gate> {
    Arc::new(Gate {
        epoch: AtomicUsize::new(0),
        reads: AtomicBool::new(true),
        reuse,
        merge,
    })
}
fn tool(wait: bool) -> Arc<ReadTool> {
    Arc::new(ReadTool {
        calls: AtomicUsize::new(0),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        wait,
    })
}
async fn begin(invocation: &ToolInvocation) -> Arc<ToolFactRecord> {
    let ToolFactAdmission::New(facts) = ToolFactRecord::begin(invocation).await.unwrap() else {
        panic!("new request")
    };
    Arc::new(facts)
}
#[tokio::test]
async fn completed_leader_cancellation_is_durable_while_follower_remains_live() {
    let first = invocation(gate(false, true)).await;
    let mut second = first.clone();
    second.call_id = "surviving-follower".into();
    second.cancellation_token = CancellationToken::new();
    let runtime = tool(false);
    let second_facts = begin(&second).await;
    let (mut follower, store, source_sequence) = {
        let first_facts = begin(&first).await;
        let mut leader = ToolDispatch::prepare(&first, runtime.clone(), first_facts.clone())
            .await
            .unwrap();
        leader.run(first.clone(), runtime.clone()).await.unwrap();
        let follower = ToolDispatch::prepare(&second, runtime.clone(), second_facts.clone())
            .await
            .unwrap();
        // The leader is cancelled after actual completion, before acceptance.
        // A surviving logical caller and the session still own the shared pool.
        (follower, first_facts.store.clone(), first_facts.sequence)
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !store.tool_waiter_cancelled(source_sequence).await.unwrap() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("completed leader cancellation must not wait for pool eviction");
    let output = follower.run(second, runtime.clone()).await.unwrap();
    assert_eq!(
        output.result.code_mode_result(&output.payload),
        serde_json::json!("verified output")
    );
    second_facts
        .accepted(snapshot(&output).unwrap())
        .await
        .unwrap();
    second_facts.offered().await.unwrap();
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    assert!(store.tool_waiter_cancelled(source_sequence).await.unwrap());
}
#[tokio::test]
async fn cancelling_all_waiters_stops_actual_and_admits_a_new_flight() {
    let first = invocation(gate(false, true)).await;
    let mut second = first.clone();
    second.call_id = "second-cancelled".into();
    second.cancellation_token = CancellationToken::new();
    let runtime = tool(true);
    let first_facts = begin(&first).await;
    let second_facts = begin(&second).await;
    let leader = ToolDispatch::prepare(&first, runtime.clone(), first_facts.clone())
        .await
        .unwrap();
    runtime.started.notified().await;
    let follower = ToolDispatch::prepare(&second, runtime.clone(), second_facts.clone())
        .await
        .unwrap();
    drop(leader);
    drop(follower);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let source = first_facts
                .store
                .tool_execution_fact(first_facts.sequence)
                .await
                .unwrap();
            if source.attempt.unwrap().execution == codex_state::ToolExecutionStatus::Uncertain
                && first_facts
                    .store
                    .tool_waiter_cancelled(first_facts.sequence)
                    .await
                    .unwrap()
                && second_facts
                    .store
                    .tool_waiter_cancelled(second_facts.sequence)
                    .await
                    .unwrap()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("last logical waiter must stop the actual execution");
    let mut next = first.clone();
    next.call_id = "new-after-cancellation".into();
    next.cancellation_token = CancellationToken::new();
    runtime.release.notify_one();
    let registry = ToolRegistry::from_tools([runtime.clone() as Arc<dyn CoreToolRuntime>]);
    let output = registry
        .dispatch_any_with_terminal_outcome(next, None)
        .await
        .unwrap();
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 2);
    let codex_protocol::models::ResponseInputItem::FunctionCallOutput { call_id, .. } =
        output.into_response()
    else {
        panic!("function output")
    };
    assert_eq!(call_id, "new-after-cancellation");
    drop(first_facts);
    drop(second_facts);
    assert!(
        registry
            .dispatch_any_with_terminal_outcome(first, None)
            .await
            .is_err()
    );
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn completed_execution_stays_shared_until_logical_settlement() {
    for reuse in [false, true] {
        let first = invocation(gate(reuse, true)).await;
        let runtime = tool(false);
        {
            let first_facts = begin(&first).await;
            let mut leader = ToolDispatch::prepare(&first, runtime.clone(), first_facts.clone())
                .await
                .unwrap();
            let output = leader.run(first.clone(), runtime.clone()).await.unwrap();

            // Actual execution is verified, while the caller still has post hooks
            // and result acceptance to complete. Another caller overlaps that work.
            let mut second = first.clone();
            second.call_id = "during-settlement".into();
            let second_facts = begin(&second).await;
            let mut follower =
                ToolDispatch::prepare(&second, runtime.clone(), second_facts.clone())
                    .await
                    .unwrap();
            let shared = follower.run(second, runtime.clone()).await.unwrap();
            assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                second_facts
                    .store
                    .tool_sharing_fact(second_facts.sequence)
                    .await
                    .unwrap()
                    .unwrap()
                    .kind,
                ToolSharingKind::Merged
            );
            first_facts
                .accepted(snapshot(&output).unwrap())
                .await
                .unwrap();
            second_facts
                .accepted(snapshot(&shared).unwrap())
                .await
                .unwrap();
        }
        // Once both logical callers settle, only explicit immutable reuse may
        // avoid a new execution. Coalescing alone must not become a cache.
        let mut next = first;
        next.call_id = "after-settlement".into();
        let registry = ToolRegistry::from_tools([runtime.clone() as Arc<dyn CoreToolRuntime>]);
        registry
            .dispatch_any_with_terminal_outcome(next, None)
            .await
            .unwrap();
        assert_eq!(
            runtime.calls.load(Ordering::SeqCst),
            if reuse { 1 } else { 2 }
        );
    }
}
#[tokio::test]
async fn cancellation_of_one_logical_waiter_preserves_the_other_delivery() {
    for cancel_leader in [true, false] {
        let first = invocation(gate(false, true)).await;
        let mut second = first.clone();
        second.call_id = "second".into();
        second.cancellation_token = CancellationToken::new();
        let runtime = tool(true);
        let first_facts = begin(&first).await;
        let second_facts = begin(&second).await;
        let leader = ToolDispatch::prepare(&first, runtime.clone(), first_facts.clone())
            .await
            .unwrap();
        runtime.started.notified().await;
        let follower = ToolDispatch::prepare(&second, runtime.clone(), second_facts.clone())
            .await
            .unwrap();
        let (mut surviving, active, facts) = if cancel_leader {
            drop(leader);
            (follower, second, second_facts.clone())
        } else {
            drop(follower);
            (leader, first, first_facts.clone())
        };
        runtime.release.notify_one();
        let output = surviving.run(active, runtime.clone()).await.unwrap();
        assert_eq!(
            output.result.code_mode_result(&output.payload),
            serde_json::json!("verified output")
        );
        facts.accepted(snapshot(&output).unwrap()).await.unwrap();
        assert!(
            facts
                .store
                .record_verified_tool_progress(
                    &facts
                        .store
                        .tool_execution_fact(facts.sequence)
                        .await
                        .unwrap()
                        .request
                        .thread_id,
                    facts.sequence
                )
                .await
                .unwrap()
        );
        facts.offered().await.unwrap();
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            first_facts
                .store
                .tool_execution_fact(first_facts.sequence)
                .await
                .unwrap()
                .attempt
                .unwrap()
                .execution,
            codex_state::ToolExecutionStatus::Completed
        );
    }
}
#[tokio::test]
async fn reuse_rebinds_protocol_id_and_account_changes_require_new_actual_execution() {
    let gate = gate(true, false);
    let first = invocation(gate.clone()).await;
    let runtime = tool(false);
    let registry = ToolRegistry::from_tools([runtime.clone() as Arc<dyn CoreToolRuntime>]);
    registry
        .dispatch_any_with_terminal_outcome(first.clone(), None)
        .await
        .unwrap();
    registry
        .dispatch_any_with_terminal_outcome(first.clone(), None)
        .await
        .unwrap();
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    let mut next = first.clone();
    next.call_id = "next".into();
    let result = registry
        .dispatch_any_with_terminal_outcome(next.clone(), None)
        .await
        .unwrap()
        .into_response();
    let codex_protocol::models::ResponseInputItem::FunctionCallOutput { call_id, .. } = result
    else {
        panic!("function output")
    };
    assert_eq!(call_id, "next");
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    registry
        .dispatch_any_with_terminal_outcome(next.clone(), None)
        .await
        .unwrap();
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    gate.epoch.store(1, Ordering::SeqCst);
    next.call_id = "new-account".into();
    registry
        .dispatch_any_with_terminal_outcome(next.clone(), None)
        .await
        .unwrap();
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 2);
    gate.reads.store(false, Ordering::SeqCst);
    next.call_id = "denied-read".into();
    assert!(
        registry
            .dispatch_any_with_terminal_outcome(next, None)
            .await
            .is_err()
    );
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 2);
    let store = ExecutionFacts::for_session(&first.session)
        .store
        .get()
        .unwrap()
        .clone();
    let events = store
        .list_tool_runtime_events(&first.session.thread_id.to_string(), 0, 200)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.fact, codex_state::ToolRuntimeFact::Progress(_)))
            .count(),
        1
    );
}
