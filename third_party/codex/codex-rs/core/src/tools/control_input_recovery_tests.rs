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
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_state::ControlInputRecovery;
use codex_state::ToolAttemptClaim;
use codex_state::ToolExecutionStatus;
use codex_state::ToolOutputDisposition;
use codex_state::ToolRequestIdentity;
use codex_state::ToolRequestObservation;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

struct ReadGate {
    allowed: AtomicBool,
    deadline: Mutex<Option<Instant>>,
    expected: Mutex<Vec<ToolResultReadRequest>>,
    revalidations: AtomicUsize,
    revoke_at: AtomicUsize,
}

impl ReadGate {
    fn new() -> Self {
        Self {
            allowed: AtomicBool::new(true),
            deadline: Mutex::new(None),
            expected: Mutex::new(Vec::new()),
            revalidations: AtomicUsize::new(0),
            revoke_at: AtomicUsize::new(0),
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

struct SeedFixture {
    _home: tempfile::TempDir,
    invocation: ToolInvocation,
    store: Arc<StateRuntime>,
    parent: ToolExecutionFact,
    input: ToolExecutionFact,
    events: async_channel::Receiver<codex_protocol::protocol::Event>,
    gate: Arc<ReadGate>,
}

struct Fixture {
    _home: tempfile::TempDir,
    invocation: ToolInvocation,
    replay: Arc<ToolFactReplay>,
    input: ToolExecutionFact,
    events: async_channel::Receiver<codex_protocol::protocol::Event>,
    gate: Arc<ReadGate>,
}

async fn seed_fixture(enabled: bool, same_owner: bool) -> SeedFixture {
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
        .observe_tool_request(
            &parent_request,
            old_owner,
            &payload_value(&invocation.payload).to_string(),
        )
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
        .observe_tool_request(
            &input_request,
            old_owner,
            &payload_value(&control.payload).to_string(),
        )
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
    let gate = Arc::new(ReadGate::new());
    session
        .services
        .thread_extension_data
        .insert(ToolCallGateAttachment::new(gate.clone()));
    SeedFixture {
        _home: home,
        invocation,
        store,
        parent,
        input,
        events,
        gate,
    }
}

async fn admit_fixture(seed: SeedFixture) -> Fixture {
    let ToolFactAdmission::Replay(replay) = ToolFactRecord::begin(&seed.invocation).await.unwrap()
    else {
        panic!("original parent replay")
    };
    let input = seed
        .store
        .tool_execution_fact(seed.input.request_sequence)
        .await
        .unwrap();
    *seed.gate.expected.lock().unwrap() = vec![
        read_request(&replay.fact, &seed.invocation.call_id).unwrap(),
        read_request(&input, &input.request.logical_id).unwrap(),
    ];
    Fixture {
        _home: seed._home,
        invocation: seed.invocation,
        replay: Arc::new(replay),
        input,
        events: seed.events,
        gate: seed.gate,
    }
}

async fn fixture(enabled: bool, same_owner: bool) -> Fixture {
    admit_fixture(seed_fixture(enabled, same_owner).await).await
}

async fn recovered_fixture() -> Fixture {
    // The private reader exercises an already completed answer. It never
    // reconstructs a waiter belonging to an abandoned Core.
    let seed = seed_fixture(true, false).await;
    let response_json =
        json!({"answers":{"answer":{"answers":["private original answer"]}}}).to_string();
    for (fact, execution, disposition, body) in [
        (
            &seed.input,
            ToolExecutionStatus::Completed,
            ToolOutputDisposition::Accepted,
            response_json.as_str(),
        ),
        (
            &seed.parent,
            ToolExecutionStatus::Uncertain,
            ToolOutputDisposition::Rejected,
            "{\"success\":false,\"state\":\"code_cell_interrupted\"}",
        ),
    ] {
        let attempt = fact.attempt.as_ref().unwrap();
        seed.store
            .complete_tool_attempt(
                fact.request_sequence,
                &attempt.attempt_id,
                &attempt.owner_id,
                execution,
                Some(body),
            )
            .await
            .unwrap();
        seed.store
            .decide_tool_output(
                fact.request_sequence,
                &attempt.attempt_id,
                &attempt.owner_id,
                disposition,
                Some(body),
            )
            .await
            .unwrap();
    }
    let fixture = admit_fixture(seed).await;
    let record = ControlInputRecovery {
        result_ref: result_ref(&fixture.replay.fact, &fixture.input),
        parent: (*fixture.replay.fact).clone(),
        input: fixture.input.clone(),
        response_digest: digest(json!(response_json)),
        response_json,
    };
    let fact = ToolReconciliationFact {
        schema_version: 1,
        request_sequence: record.parent.request_sequence,
        thread_id: record.parent.request.thread_id.clone(),
        observer_id: fixture.replay.owner_id.clone(),
        attempt: record.parent.attempt.clone().unwrap(),
        evidence: ToolRecoveryEvidence::ControlInputResolved {
            input_request_sequence: record.input.request_sequence,
            input_operation_digest: record
                .input
                .attempt
                .as_ref()
                .unwrap()
                .operation_digest
                .clone(),
            response_digest: record.response_digest.clone(),
            result_ref: record.result_ref.clone(),
            continuation_lost: true,
        },
    };
    fixture
        .replay
        .store
        .record_control_input_recovery(
            &record,
            &super::super::handlers::request_user_input_result::prepare_pages(&record).unwrap(),
            &fact,
        )
        .await
        .unwrap();
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
async fn abandoned_control_input_fails_once_and_rejects_late_answer_without_writes() {
    let fixture = fixture(true, false).await;
    for fact in [&*fixture.replay.fact, &fixture.input] {
        let attempt = fact.attempt.as_ref().unwrap();
        assert_eq!(attempt.execution, ToolExecutionStatus::Uncertain);
        assert_eq!(attempt.disposition, ToolOutputDisposition::Rejected);
        assert!(attempt.execution_result_retained);
    }
    let store = &fixture.replay.store;
    let thread = &fixture.input.request.thread_id;
    let before = store
        .list_tool_runtime_events(thread, 0, 100)
        .await
        .unwrap();
    for _ in 0..2 {
        assert!(
            recover(&fixture.replay, &fixture.invocation, Some(&OldExec))
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(
        !fixture
            .invocation
            .session
            .notify_user_input_response(
                &fixture.input.request.turn_id,
                &fixture.input.request.logical_id,
                RequestUserInputResponse {
                    answers: HashMap::from([(
                        "answer".into(),
                        RequestUserInputAnswer {
                            answers: vec!["late private answer".into()]
                        }
                    )])
                }
            )
            .await
    );
    for fact in [&*fixture.replay.fact, &fixture.input] {
        let attempt = fact.attempt.as_ref().unwrap();
        assert!(
            store
                .complete_tool_attempt(
                    fact.request_sequence,
                    &attempt.attempt_id,
                    &attempt.owner_id,
                    ToolExecutionStatus::Completed,
                    Some("late success")
                )
                .await
                .is_err()
        );
        assert_eq!(
            store
                .tool_execution_fact(fact.request_sequence)
                .await
                .unwrap(),
            *fact
        );
    }
    assert_eq!(
        store
            .list_tool_runtime_events(thread, 0, 100)
            .await
            .unwrap(),
        before
    );
    assert!(fixture.events.try_recv().is_err());
    assert_eq!(fixture.gate.revalidations.load(Ordering::SeqCst), 0);
    assert!(
        store
            .control_input_recovery_header(
                thread,
                &result_ref(&fixture.replay.fact, &fixture.input)
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn abandoned_control_input_waits_for_sqlite_writer_and_atomically_fails_call_pairs() {
    let seed = seed_fixture(true, false).await;
    let thread = seed.input.request.thread_id.clone();
    let store = Arc::clone(&seed.store);
    let pool = store
        .sqlite()
        .open_read_write_pool(&store.sqlite().state_db_path())
        .await
        .unwrap();
    let pairs = || {
        sqlx::query("SELECT original_input,call_status,call_result FROM tool_requests WHERE thread_id=? ORDER BY sequence").bind(&thread)
    };
    let before = pairs().fetch_all(&pool).await.unwrap();
    assert_eq!(before.len(), 2);
    assert!(
        before
            .iter()
            .all(|row| row.get::<String, _>("call_status") == "pending"
                && row.get::<Option<String>, _>("call_result").is_none())
    );
    let lock = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let admission = tokio::spawn(admit_fixture(seed));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if store.try_tool_owner_lock(&thread).unwrap().is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("new Core acquired the exclusive owner lock");
    assert!(
        !admission.is_finished(),
        "real SQLite writer lock holds startup settlement"
    );
    let blocked = pairs().fetch_all(&pool).await.unwrap();
    assert!(
        blocked
            .iter()
            .all(|row| row.get::<String, _>("call_status") == "pending"
                && row.get::<Option<String>, _>("call_result").is_none())
    );
    lock.commit().await.unwrap();
    let fixture = tokio::time::timeout(Duration::from_secs(5), admission)
        .await
        .unwrap()
        .unwrap();
    let after = pairs().fetch_all(&pool).await.unwrap();
    assert_eq!(after.len(), 2);
    for (original, failed) in before.iter().zip(&after) {
        assert_eq!(
            failed.get::<String, _>("original_input"),
            original.get::<String, _>("original_input")
        );
        assert_eq!(failed.get::<String, _>("call_status"), "failed");
        let result: Value = serde_json::from_str(&failed.get::<String, _>("call_result")).unwrap();
        assert_eq!(
            result,
            json!({"status":"failed","code":"tool_execution_interrupted","effects":"unknown"})
        );
    }
    assert!(
        recover(&fixture.replay, &fixture.invocation, Some(&OldExec))
            .await
            .unwrap()
            .is_none()
    );
    assert!(fixture.events.try_recv().is_err());
    assert_eq!(
        store.fail_pending_tool_calls(&thread, None).await.unwrap(),
        0
    );
    pool.close().await;
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

#[tokio::test]
async fn control_input_result_reader_expired_authority_rejects_private_answer() {
    let fixture = recovered_fixture().await;
    *fixture.gate.deadline.lock().unwrap() = Some(Instant::now());
    let before = fixture.gate.revalidations.load(Ordering::SeqCst);
    let denied = RequestUserInputResultHandler
        .handle(reader_invocation(&fixture))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(denied.contains("result_read_denied"));
    assert!(!denied.contains("private original answer"));
    assert_eq!(fixture.gate.revalidations.load(Ordering::SeqCst), before);
}
