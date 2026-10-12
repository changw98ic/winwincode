// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::ToolCallGate;
use crate::ToolCallGateAuthorization;
use crate::ToolCallGateRejection;
use crate::ToolCallGateRequest;
use crate::session::step_context::StepContext;
use crate::state::ActiveTurn;
use crate::tools::context::ToolCallSource;
use crate::tools::execution_facts::ExecutionFacts;
use crate::tools::execution_facts::ToolFactAdmission;
use crate::tools::execution_facts::ToolFactRecord;
use crate::tools::execution_facts::operation_digest;
use crate::tools::execution_facts::payload_value;
use crate::tools::handlers::request_user_input_result::RequestUserInputResultHandler;
use codex_protocol::config_types::ModeKind;
use codex_protocol::protocol::EventMsg;
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_state::ToolAttemptClaim;
use codex_state::ToolRequestIdentity;
use codex_state::ToolRequestObservation;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

struct ReadGate {
    allowed: AtomicBool,
    deadline: Mutex<Option<Instant>>,
    expected: Mutex<Vec<ToolResultReadRequest>>,
    revalidations: AtomicUsize,
    revoke_at: AtomicUsize,
    checked: Notify,
}

impl ReadGate {
    fn new() -> Self {
        Self {
            allowed: AtomicBool::new(true),
            deadline: Mutex::new(None),
            expected: Mutex::new(Vec::new()),
            revalidations: AtomicUsize::new(0),
            revoke_at: AtomicUsize::new(0),
            checked: Notify::new(),
        }
    }

    fn check(&self, request: &ToolResultReadRequest) -> Result<(), ToolCallGateRejection> {
        let exact = self.expected.lock().unwrap().contains(request);
        let within_deadline = self
            .deadline
            .lock()
            .unwrap()
            .is_none_or(|deadline| Instant::now() < deadline);
        if exact && within_deadline && self.allowed.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(ToolCallGateRejection::new(
                "READ_EXPIRED",
                "read authority expired",
            ))
        }
    }

    async fn wait_for_revalidations(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = self.checked.notified();
                if self.revalidations.load(Ordering::SeqCst) >= count {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("recovery reached the exact read gate");
    }
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
        request: ToolResultReadRequest,
    ) -> BoxFuture<'static, Result<ToolCallGateAuthorization, ToolCallGateRejection>> {
        let result = self
            .check(&request)
            .map(|()| ToolCallGateAuthorization::default());
        Box::pin(async move { result })
    }
    fn revalidate_result_read(
        &self,
        request: ToolResultReadRequest,
        _: ToolCallGateAuthorization,
    ) -> BoxFuture<'static, Result<(), ToolCallGateRejection>> {
        let count = self.revalidations.fetch_add(1, Ordering::SeqCst) + 1;
        if self.revoke_at.load(Ordering::SeqCst) == count {
            self.allowed.store(false, Ordering::SeqCst);
        }
        let result = self.check(&request);
        self.checked.notify_one();
        Box::pin(async move { result })
    }
}

struct OldExec;
impl ToolExecutor<ToolInvocation> for OldExec {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("exec").with_default_namespace()
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "exec".into(),
            description: "original script fixture".into(),
            strict: false,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
        })
    }
    fn handle(&self, _: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        panic!("control recovery must never execute the old script")
    }
}
impl CoreToolRuntime for OldExec {
    fn authorization_policy(&self) -> super::super::authorization::AuthorizationPolicy {
        super::super::authorization::AuthorizationPolicy::CoreControl
    }
}

struct Fixture {
    _home: tempfile::TempDir,
    invocation: ToolInvocation,
    replay: Arc<ToolFactReplay>,
    input: ToolExecutionFact,
    events: async_channel::Receiver<codex_protocol::protocol::Event>,
    gate: Arc<ReadGate>,
}

async fn fixture(enabled: bool, same_owner: bool) -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let sqlite = codex_state::SqliteConfig::new_for_testing(
        codex_utils_absolute_path::AbsolutePathBuf::try_from(home.path()).unwrap(),
    );
    let store = StateRuntime::init(sqlite, "test".into()).await.unwrap();
    let (session, mut turn, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let context = Arc::get_mut(&mut turn).unwrap();
    context.mode = ModeKind::Plan;
    Arc::make_mut(&mut context.config).experimental_request_user_input_enabled = enabled;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let service = ExecutionFacts::for_session(&session);
    assert!(service.store.set(Arc::clone(&store)).is_ok());
    assert!(service.owner_id.set("current-owner".into()).is_ok());
    let invocation = ToolInvocation {
        session: Arc::clone(&session),
        turn: Arc::clone(&turn),
        step_context: StepContext::for_test(turn),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::default(),
        )),
        call_id: "original-parent-call".into(),
        tool_name: OldExec.tool_name(),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: json!({"code":"await originalInput(); oldScriptMarker();"}).to_string(),
        },
    };
    let old_owner = if same_owner {
        "current-owner"
    } else {
        "original-owner"
    };
    let parent_logical_id = format!(
        "direct:{}",
        digest(json!([invocation.turn.sub_id, invocation.call_id]))
    );
    let parent_request = ToolRequestIdentity {
        thread_id: session.thread_id.to_string(),
        logical_id: parent_logical_id.clone(),
        turn_id: invocation.turn.sub_id.clone(),
        scope_id: invocation.turn.sub_id.clone(),
        cell_id: None,
        parent_call_id: None,
        tool_name: invocation.tool_name.to_string(),
        source: "model".into(),
        binding: digest(payload_value(&invocation.payload)),
    };
    let ToolRequestObservation::New(parent_observed) = store
        .observe_tool_request(&parent_request, "test-owner", "{}")
        .await
        .unwrap()
    else {
        panic!("new parent")
    };
    let ToolAttemptClaim::Claimed(parent) = store
        .claim_tool_attempt(
            parent_observed.request_sequence,
            "original-parent-attempt",
            old_owner,
            &operation_digest(&invocation, &OldExec),
            &payload_value(&invocation.payload).to_string(),
        )
        .await
        .unwrap()
    else {
        panic!("claimed parent")
    };
    store
        .open_tool_cell(
            &parent.request.thread_id,
            &parent_logical_id,
            "original-cell",
            "original-scope",
            old_owner,
        )
        .await
        .unwrap();
    let input_handler = RequestUserInputHandler {
        available_modes: codex_tools::request_user_input_available_modes(
            &invocation.turn.config.features,
        ),
    };
    let input_payload = ToolPayload::Function { arguments: json!({"questions":[{
        "id":"answer","header":"Choice","question":"Choose an answer",
        "options":[{"label":"A","description":"Choice A"},{"label":"B","description":"Choice B"}]
    }]}).to_string() };
    let input_id = super::super::execution_facts::nested_identity(
        "original-cell",
        "original-child",
        "original-scope",
    );
    let mut control = invocation.clone();
    control.call_id = input_id.clone();
    control.tool_name = input_handler.tool_name();
    control.payload = input_payload;
    let input_request = ToolRequestIdentity {
        thread_id: session.thread_id.to_string(),
        logical_id: input_id,
        turn_id: invocation.turn.sub_id.clone(),
        scope_id: "original-scope".into(),
        cell_id: Some("original-cell".into()),
        parent_call_id: Some(invocation.call_id.clone()),
        tool_name: input_handler.tool_name().to_string(),
        source: "code_mode".into(),
        binding: digest(payload_value(&control.payload)),
    };
    let ToolRequestObservation::New(input_observed) = store
        .observe_tool_request(&input_request, "test-owner", "{}")
        .await
        .unwrap()
    else {
        panic!("new input")
    };
    let ToolAttemptClaim::Claimed(input) = store
        .claim_tool_attempt(
            input_observed.request_sequence,
            "original-input-attempt",
            old_owner,
            &operation_digest(&control, &input_handler),
            &payload_value(&control.payload).to_string(),
        )
        .await
        .unwrap()
    else {
        panic!("claimed input")
    };
    let ToolFactAdmission::Replay(replay) = ToolFactRecord::begin(&invocation).await.unwrap()
    else {
        panic!("original parent replay")
    };
    let gate = Arc::new(ReadGate::new());
    *gate.expected.lock().unwrap() = vec![
        read_request(&parent, &invocation.call_id).unwrap(),
        read_request(&input, &input.request.logical_id).unwrap(),
    ];
    session
        .services
        .thread_extension_data
        .insert(ToolCallGateAttachment::new(gate.clone()));
    Fixture {
        _home: home,
        invocation,
        replay: Arc::new(replay),
        input,
        events,
        gate,
    }
}

fn start_recovery(
    fixture: &Fixture,
) -> tokio::task::JoinHandle<Result<Option<FunctionCallError>, FunctionCallError>> {
    let replay = Arc::clone(&fixture.replay);
    let invocation = fixture.invocation.clone();
    tokio::spawn(async move { recover(&replay, &invocation, Some(&OldExec)).await })
}

async fn answer(fixture: &Fixture, value: &str) {
    let event = tokio::time::timeout(Duration::from_secs(5), fixture.events.recv())
        .await
        .unwrap()
        .unwrap();
    let EventMsg::RequestUserInput(request) = event.msg else {
        panic!("only the original input waiter event")
    };
    assert_eq!(request.turn_id, fixture.input.request.turn_id);
    assert_eq!(request.call_id, fixture.input.request.logical_id);
    assert!(
        fixture
            .invocation
            .session
            .notify_user_input_response(
                &request.turn_id,
                &request.call_id,
                RequestUserInputResponse {
                    answers: HashMap::from([(
                        "answer".into(),
                        RequestUserInputAnswer {
                            answers: vec![value.into()]
                        }
                    )])
                }
            )
            .await
    );
}

async fn recovered_fixture() -> Fixture {
    let fixture = fixture(true, false).await;
    let recovery = start_recovery(&fixture);
    answer(&fixture, "private original answer").await;
    let output = recovery.await.unwrap().unwrap().unwrap().to_string();
    assert!(output.contains("inputConsumed"));
    fixture
}

fn reader_invocation(fixture: &Fixture) -> ToolInvocation {
    let mut invocation = fixture.invocation.clone();
    invocation.call_id = "private-page-read".into();
    invocation.tool_name = RequestUserInputResultHandler.tool_name();
    invocation.payload = ToolPayload::Function {
        arguments:
            json!({"resultRef": result_ref(&fixture.replay.fact, &fixture.input), "cursor":"0"})
                .to_string(),
    };
    invocation
}

#[tokio::test]
async fn control_input_recovery_disabled_current_catalog_never_installs_waiter() {
    let fixture = fixture(false, false).await;
    assert!(
        recover(&fixture.replay, &fixture.invocation, Some(&OldExec))
            .await
            .unwrap()
            .is_none()
    );
    assert!(fixture.events.try_recv().is_err());
    assert_eq!(fixture.gate.revalidations.load(Ordering::SeqCst), 0);
    assert!(
        fixture
            .replay
            .store
            .control_input_recovery_header(
                &fixture.input.request.thread_id,
                &result_ref(&fixture.replay.fact, &fixture.input)
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn control_input_recovery_same_owner_never_recreates_live_waiter() {
    let fixture = fixture(true, true).await;
    assert!(
        recover(&fixture.replay, &fixture.invocation, Some(&OldExec))
            .await
            .unwrap()
            .is_none()
    );
    assert!(fixture.events.try_recv().is_err());
    assert_eq!(fixture.gate.revalidations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn control_input_recovery_concurrent_parent_claim_consumes_one_waiter_and_publishes_once() {
    let fixture = fixture(true, false).await;
    let first = start_recovery(&fixture);
    fixture.gate.wait_for_revalidations(2).await;
    assert!(
        recover(&fixture.replay, &fixture.invocation, Some(&OldExec))
            .await
            .unwrap()
            .is_none()
    );
    answer(&fixture, "one exact answer").await;
    assert!(
        first
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .to_string()
            .contains("inputConsumed")
    );
    assert!(fixture.events.try_recv().is_err());
    let events = fixture
        .replay
        .store
        .list_tool_runtime_events(&fixture.input.request.thread_id, 0, 100)
        .await
        .unwrap();
    let publications = events
        .iter()
        .filter(|event| {
            matches!(&event.fact, codex_state::ToolRuntimeFact::Reconciliation(fact)
        if matches!(&fact.evidence, ToolRecoveryEvidence::ControlInputResolved { .. }))
        })
        .count();
    assert_eq!(publications, 1);
    assert_eq!(
        fixture
            .replay
            .store
            .tool_execution_fact(fixture.input.request_sequence)
            .await
            .unwrap(),
        fixture.input
    );
    assert_eq!(
        fixture
            .replay
            .store
            .tool_execution_fact(fixture.replay.fact.request_sequence)
            .await
            .unwrap(),
        *fixture.replay.fact
    );
}

#[tokio::test]
async fn control_input_recovery_sqlite_writer_wait_crosses_deadline_retains_history_and_denies_delivery()
 {
    for answer_bytes in [32, 20_000] {
        let fixture = fixture(true, false).await;
        let sqlite = fixture.replay.store.sqlite();
        let lock_pool = sqlite
            .open_read_write_pool(&sqlite.state_db_path())
            .await
            .unwrap();
        let lock = lock_pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let recovery = start_recovery(&fixture);
        let private_answer = "X".repeat(answer_bytes);
        answer(&fixture, &private_answer).await;
        fixture.gate.wait_for_revalidations(4).await;
        *fixture.gate.deadline.lock().unwrap() = Some(Instant::now() + Duration::from_millis(100));
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            !recovery.is_finished(),
            "real SQLite writer lock must keep COMMIT pending"
        );
        let reference = result_ref(&fixture.replay.fact, &fixture.input);
        assert!(
            fixture
                .replay
                .store
                .control_input_recovery_header(&fixture.input.request.thread_id, &reference)
                .await
                .unwrap()
                .is_none()
        );
        lock.commit().await.unwrap();
        let denied = tokio::time::timeout(Duration::from_secs(5), recovery)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(denied.contains("result_read_denied"));
        assert!(
            !denied.contains("input-result:")
                && !denied.contains("answers")
                && !denied.contains(&private_answer)
        );
        let private = fixture
            .replay
            .store
            .control_input_recovery(&fixture.input.request.thread_id, &reference)
            .await
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_str(&private.response_json).unwrap();
        assert_eq!(value["answers"]["answer"]["answers"][0], private_answer);
        let header = fixture
            .replay
            .store
            .control_input_recovery_header(&fixture.input.request.thread_id, &reference)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(header.total_bytes, private.response_json.len());
        assert!(header.page_count > 0);
        assert!(
            fixture
                .replay
                .store
                .control_input_result_page(&fixture.input.request.thread_id, &reference, "0")
                .await
                .unwrap()
                .is_some()
        );
        let events = fixture
            .replay
            .store
            .list_tool_runtime_events(&fixture.input.request.thread_id, 0, 100)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(
                    |event| matches!(&event.fact, codex_state::ToolRuntimeFact::Reconciliation(fact)
            if matches!(&fact.evidence, ToolRecoveryEvidence::ControlInputResolved { .. }))
                )
                .count(),
            1
        );
        assert_eq!(
            fixture
                .replay
                .store
                .tool_execution_fact(fixture.input.request_sequence)
                .await
                .unwrap(),
            fixture.input
        );
        assert!(
            RequestUserInputResultHandler
                .handle(reader_invocation(&fixture))
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("result_read_denied")
        );
        lock_pool.close().await;
    }
}

#[tokio::test]
async fn control_input_result_reader_old_turn_rejects_original_private_page() {
    let fixture = recovered_fixture().await;
    let mut invocation = reader_invocation(&fixture);
    let (_other_session, mut old_turn) = crate::session::tests::make_session_and_context().await;
    old_turn.sub_id = "another-turn".into();
    invocation.turn = Arc::new(old_turn);
    invocation.step_context = StepContext::for_test(Arc::clone(&invocation.turn));
    let before = fixture.gate.revalidations.load(Ordering::SeqCst);
    let denied = RequestUserInputResultHandler
        .handle(invocation)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(denied.contains("control_input_result_scope_expired"));
    assert!(!denied.contains("private original answer"));
    assert_eq!(fixture.gate.revalidations.load(Ordering::SeqCst), before);
}

#[tokio::test]
async fn control_input_result_reader_authority_revoked_after_private_read_rejects_delivery() {
    let fixture = recovered_fixture().await;
    let before = fixture.gate.revalidations.load(Ordering::SeqCst);
    fixture.gate.revoke_at.store(before + 1, Ordering::SeqCst);
    let denied = RequestUserInputResultHandler
        .handle(reader_invocation(&fixture))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(denied.contains("result_read_denied"));
    assert!(!denied.contains("private original answer"));
    assert_eq!(
        fixture.gate.revalidations.load(Ordering::SeqCst),
        before + 1
    );
    assert!(
        fixture
            .replay
            .store
            .control_input_recovery_header(
                &fixture.input.request.thread_id,
                &result_ref(&fixture.replay.fact, &fixture.input)
            )
            .await
            .unwrap()
            .is_some()
    );
}
