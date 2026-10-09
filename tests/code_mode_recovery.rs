//! Rebuild the product Kernel at an observed model boundary, then resume its rollout.
use super::*;
use sha2::{Digest, Sha256};
use winwincode_kernel::{
    KernelToolResultReadRequest, ToolDependencySnapshot, ToolInputContext, ToolInputProof,
    ToolInputProofRequest,
};

fn state_database(fixture: &Fixture) -> rusqlite::Connection {
    std::fs::read_dir(fixture.0.join("home"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "sqlite")
        })
        .find_map(|path| {
            let database = rusqlite::Connection::open(path).unwrap();
            database
                .query_row(
                    "SELECT name FROM sqlite_master WHERE name='tool_diagnostic_feedback'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()
                .map(|_| database)
        })
        .expect("Core state database")
}

#[derive(Debug)]
struct ResumeModel {
    cell: String,
    requests: Mutex<Vec<Value>>,
    explanation: Mutex<Option<Value>>,
}

impl ModelPort for ResumeModel {
    fn stream(
        &self,
        request: ModelPortRequest,
    ) -> BoxFuture<'static, Result<ModelPortStream, ModelPortFailure>> {
        let payload: Value = serde_json::from_str(&request.payload_json).unwrap();
        let mut requests = self.requests.lock().unwrap();
        let index = requests.len();
        assert_eq!(
            payload["request"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["exec", "wait"]
        );
        requests.push(payload);
        let items = match index {
            0 => vec![call_item(
                "wait",
                "resume-old-cell",
                &json!({
                    "cell_id":self.cell,"yield_time_ms":1000
                }),
            )],
            1 => {
                let input = requests[1]["request"]["input"].as_array().unwrap();
                let output = input
                    .iter()
                    .rev()
                    .find(|item| item["type"] == "function_call_output")
                    .expect("resumed wait output");
                let diagnostics = sharing::diagnostic_payloads(&[output["output"].clone()]);
                assert!(
                    !diagnostics.is_empty(),
                    "resumed model must receive evidence"
                );
                let ids = diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic["diagnostic_id"].as_str().unwrap())
                    .collect::<Vec<_>>()
                    .join(",");
                let explanation = json!({"type":"message","role":"assistant","phase":"commentary",
                    "content":[{"type":"output_text","text":format!(
                        "I received diagnostic {ids}. The prior cell is closed after Kernel shutdown. I will use a new cell and preserve the six completed MCP calls.")} ]});
                *self.explanation.lock().unwrap() = Some(explanation.clone());
                vec![
                    explanation,
                    call_item(
                        "exec",
                        "resume-new-cell",
                        &json!("text('recovery-new-cell-ok');"),
                    ),
                ]
            }
            2 => vec![
                json!({"type":"message","role":"assistant","phase":"final_answer",
                "content":[{"type":"output_text","text":"Recovery assessed; the new cell completed."}]}),
            ],
            _ => panic!("unexpected recovery model request {index}"),
        };
        let final_answer = index == 2;
        let mut frames = vec![Ok(json!({"type":"created"}).to_string())];
        frames.extend(
            items
                .into_iter()
                .map(|item| Ok(json!({"type":"output_item_done","item":item}).to_string())),
        );
        frames.push(Ok(json!({"type":"completed","responseId":format!("resume-{index}"),"endTurn":final_answer}).to_string()));
        Box::pin(async move { Ok(Box::pin(futures::stream::iter(frames)) as ModelPortStream) })
    }
}

fn diagnostic_fault_model() -> Arc<sharing::DiagnosticOfferFaultModel> {
    Arc::new(sharing::DiagnosticOfferFaultModel {
        script: ScriptedModel {
            requests: Mutex::new(Vec::new()),
            source: DIAGNOSTIC_SOURCE,
            exec_count: 1,
            cancel_boundary: None,
            cancelled_cell: Mutex::new(None),
            direct_probe: false,
        },
        output_seen: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
    })
}

async fn rebuild_diagnostic_boundary(queued: bool) {
    let fixture = Fixture::new();
    let model = diagnostic_fault_model();
    let gate = Arc::new(RecordingGate::default());
    let kernel = Kernel::new(fixture.kernel_options(), model.clone(), gate.clone()).unwrap();
    let session = kernel
        .create_session(fixture.session_options())
        .await
        .unwrap();
    let database = state_database(&fixture);
    if queued {
        // Stop delivery before staging, after real tool calls have queued the question.
        database.execute_batch("CREATE TRIGGER pause_diagnostic_staging BEFORE INSERT ON tool_diagnostic_feedback BEGIN SELECT RAISE(FAIL, 'diagnostic staging interrupted'); END;").unwrap();
    }
    kernel
        .submit_turn(
            &session.session_id,
            "Assess the repeated operation".into(),
            TurnSubmissionOptions::default(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), model.output_seen.notified())
        .await
        .expect("initial exec output reached the model boundary");
    let outputs = latest_exec_outputs(&model.script);
    let before = all_facts(&kernel, &session.session_id).await;
    let (cell, original) = assert_initial_diagnostic_boundary(&outputs, &before, queued);
    let oracle = std::fs::read(fixture.0.join("echo-calls.jsonl")).unwrap();
    assert_eq!(
        oracle
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count(),
        6
    );
    kernel.shutdown().await.unwrap();
    drop(kernel);
    if queued {
        database
            .execute_batch("DROP TRIGGER pause_diagnostic_staging;")
            .unwrap();
    }
    let resumed_model = Arc::new(ResumeModel {
        cell: cell.clone(),
        requests: Mutex::new(Vec::new()),
        explanation: Mutex::new(None),
    });
    let rebuilt = Kernel::new(
        fixture.kernel_options(),
        resumed_model.clone(),
        gate.clone(),
    )
    .unwrap();
    let rollout_path = PathBuf::from(session.rollout_path.unwrap());
    let resumed = rebuilt
        .resume_session(rollout_path.clone(), fixture.session_options())
        .await
        .unwrap();
    assert_eq!(resumed.session_id, session.session_id);
    rebuilt
        .submit_turn(
            &resumed.session_id,
            "Assess the recorded diagnosis and resume safely".into(),
            TurnSubmissionOptions::default(),
        )
        .await
        .unwrap();
    wait_for_kind(&rebuilt, &resumed.session_id, "turn_complete").await;
    rebuilt.close_session(&resumed.session_id).await.unwrap();
    let delivered = assert_resumed_diagnostic_outputs(&resumed_model, &original, queued);
    let after = all_facts(&rebuilt, &resumed.session_id).await;
    assert_original_diagnostic_delivery(&after, &original);
    assert_explanation_response(&resumed_model, &rollout_path, &after, delivered);
    assert!(after.iter().any(|fact| fact["kind"] == "cell"
        && fact["fact"]["cell_id"] == cell
        && fact["fact"]["lifecycle"] == "closed"));
    assert_eq!(
        std::fs::read(fixture.0.join("echo-calls.jsonl")).unwrap(),
        oracle,
        "recovery must not execute the six MCP calls again"
    );
    assert_eq!(
        gate.0
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.tool_name.ends_with("echo"))
            .count(),
        6
    );
    rebuilt.shutdown().await.unwrap();
}

fn assert_initial_diagnostic_boundary(
    outputs: &[Value],
    before: &[Value],
    queued: bool,
) -> (String, Value) {
    let output = serde_json::to_string(outputs).unwrap();
    let (_, tail) = output
        .split_once("Script running with cell ID ")
        .expect("live cell boundary");
    let cell = tail
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .next()
        .unwrap()
        .to_owned();
    let original = before
        .iter()
        .find(|fact| fact["kind"] == "diagnostic" && fact["fact"]["delivery"] == "queued")
        .expect("real calls queued a diagnostic")["fact"]["diagnostic"]
        .clone();
    assert_eq!(original["kind"], "repeated_operation");
    assert_eq!(
        before
            .iter()
            .filter(|fact| fact["kind"] == "diagnostic" && fact["fact"]["delivery"] == "offered")
            .count(),
        usize::from(!queued)
    );
    assert_eq!(sharing::diagnostic_payloads(outputs).is_empty(), queued);
    assert!(
        !before
            .iter()
            .any(|fact| fact["kind"] == "diagnostic_response"),
        "offering a question is not a model explanation"
    );
    (cell, original)
}

fn assert_resumed_diagnostic_outputs(
    model: &ResumeModel,
    original: &Value,
    queued: bool,
) -> Vec<Value> {
    let requests = model.requests.lock().unwrap().clone();
    let input = requests.last().unwrap()["request"]["input"]
        .as_array()
        .unwrap();
    let resume_start = input
        .iter()
        .position(|item| item["call_id"] == "resume-old-cell")
        .unwrap();
    let current_outputs = input[resume_start + 1..]
        .iter()
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call_output" | "custom_tool_call_output")
            )
        })
        .map(|item| item["output"].clone())
        .collect::<Vec<_>>();
    let history = serde_json::to_string(&current_outputs).unwrap();
    assert!(
        history.contains("cell_closed"),
        "old cell must not appear live: {history}"
    );
    assert!(history.contains("recovery-new-cell-ok"));
    assert!(
        !history.contains("diagnosis-cell-resumed"),
        "old V8 execution cannot survive process reconstruction"
    );
    let delivered = sharing::diagnostic_payloads(&current_outputs);
    let original_deliveries = delivered
        .iter()
        .filter(|diagnostic| diagnostic["diagnostic_id"] == original["diagnostic_id"])
        .collect::<Vec<_>>();
    assert_eq!(original_deliveries.len(), usize::from(queued));
    if queued {
        assert_eq!(
            original_deliveries[0]["evidence_version"],
            original["evidence_version"]
        );
    }
    delivered
}

fn assert_original_diagnostic_delivery(after: &[Value], original: &Value) {
    let offers = after
        .iter()
        .filter(|fact| {
            fact["kind"] == "diagnostic"
                && fact["fact"]["delivery"] == "offered"
                && fact["fact"]["diagnostic"]["diagnostic_id"] == original["diagnostic_id"]
        })
        .collect::<Vec<_>>();
    assert_eq!(
        offers.len(),
        1,
        "same evidence version is offered once across reconstruction"
    );
    assert_eq!(
        offers[0]["fact"]["diagnostic"], *original,
        "Core evidence and parent/cell identities survive unchanged"
    );
}

fn assert_explanation_response(
    model: &ResumeModel,
    rollout_path: &std::path::Path,
    after: &[Value],
    delivered: Vec<Value>,
) {
    let expected_explanation = model.explanation.lock().unwrap().clone().unwrap();
    // Use the actual persisted assistant item, including its transport-assigned identity.
    let explanation = std::fs::read_to_string(rollout_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|record| {
            record["type"] == "response_item"
                && record["payload"]["role"] == "assistant"
                && record["payload"]["content"] == expected_explanation["content"]
        })
        .expect("Core persisted the actual explanation")["payload"]
        .clone();
    assert_eq!(explanation["phase"], "commentary");
    let explanation: codex_protocol::models::ResponseItem =
        serde_json::from_value(explanation).unwrap();
    let mut explanation = serde_json::to_value(explanation).unwrap();
    // History adds the turn metadata after observing the response. The response
    // fact separately stores that turn identity and hashes the incoming item.
    explanation
        .as_object_mut()
        .unwrap()
        .remove("internal_chat_message_metadata_passthrough");
    explanation.sort_all_objects();
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&explanation).unwrap())
    );
    for diagnostic in delivered {
        let offer = after
            .iter()
            .find(|fact| {
                fact["kind"] == "diagnostic"
                    && fact["fact"]["delivery"] == "offered"
                    && fact["fact"]["diagnostic"]["diagnostic_id"] == diagnostic["diagnostic_id"]
                    && fact["fact"]["diagnostic"]["evidence_version"]
                        == diagnostic["evidence_version"]
            })
            .unwrap();
        assert!(
            after
                .iter()
                .any(|fact| fact["kind"] == "diagnostic_response"
                    && fact["fact"]["diagnostic_id"] == diagnostic["diagnostic_id"]
                    && fact["fact"]["evidence_version"] == diagnostic["evidence_version"]
                    && fact["fact"]["boundary_request_sequence"]
                        == offer["fact"]["boundary_request_sequence"]
                    && fact["fact"]["response_digest"] == digest),
            "actual assistant explanation must reference the delivered question: expected={digest}, item={explanation}, responses={:?}",
            after
                .iter()
                .filter(|fact| fact["kind"] == "diagnostic_response")
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn native_queued_diagnostic_survives_kernel_reconstruction() {
    run_native_test(|runtime| runtime.block_on(rebuild_diagnostic_boundary(true)));
}

#[test]
fn native_offered_diagnostic_is_not_reoffered_after_kernel_reconstruction() {
    run_native_test(|runtime| runtime.block_on(rebuild_diagnostic_boundary(false)));
}

struct RecoveryReadGate {
    policy: sharing::SharingGate,
    allowed: std::sync::atomic::AtomicBool,
    reads: Mutex<Vec<KernelToolResultReadRequest>>,
}
impl KernelActionGate for RecoveryReadGate {
    fn freeze_tool_input(
        &self,
        context: ToolInputContext,
    ) -> BoxFuture<'static, Option<ToolDependencySnapshot>> {
        self.policy.freeze_tool_input(context)
    }
    fn verify_tool_input(
        &self,
        request: ToolInputProofRequest,
    ) -> BoxFuture<'static, Option<ToolInputProof>> {
        self.policy.verify_tool_input(request)
    }
    fn authorize(
        &self,
        request: KernelActionRequest,
    ) -> BoxFuture<'static, Result<KernelActionAuthorization, KernelFailure>> {
        self.policy.authorize(request)
    }
    fn revalidate(
        &self,
        request: KernelActionRequest,
        authorization: KernelActionAuthorization,
    ) -> BoxFuture<'static, Result<(), KernelFailure>> {
        self.policy.revalidate(request, authorization)
    }
    fn authorize_result_read(
        &self,
        request: KernelToolResultReadRequest,
    ) -> BoxFuture<'static, Result<KernelActionAuthorization, KernelFailure>> {
        self.reads.lock().unwrap().push(request.clone());
        if self.allowed.load(Ordering::SeqCst) {
            self.policy.authorize_result_read(request)
        } else {
            Box::pin(async { Err(KernelFailure::action_rejected()) })
        }
    }
    fn revalidate_result_read(
        &self,
        request: KernelToolResultReadRequest,
        authorization: KernelActionAuthorization,
    ) -> BoxFuture<'static, Result<(), KernelFailure>> {
        if self.allowed.load(Ordering::SeqCst) {
            self.policy.revalidate_result_read(request, authorization)
        } else {
            Box::pin(async { Err(KernelFailure::action_rejected()) })
        }
    }
}

#[test]
fn native_rebuilt_kernel_rechecks_read_permission_before_reusing_result() {
    run_native_test(|runtime| {
        runtime.block_on(async {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("workspace/source.txt"), "A").unwrap();
        let gate = Arc::new(RecoveryReadGate {
            policy: sharing::SharingGate {
                workspace: fixture.0.join("workspace"),
                flip_on_freeze: std::sync::atomic::AtomicBool::new(false),
            },
            allowed: std::sync::atomic::AtomicBool::new(true),
            reads: Mutex::new(Vec::new()),
        });
        let model = Arc::new(ScriptedModel {
            requests: Mutex::new(Vec::new()),
            source: "const smoke=ALL_TOOLS.find(t=>t.name.endsWith('fixture__public_smoke')); text(await tools[smoke.name]({}));",
            exec_count: 1, cancel_boundary: None, cancelled_cell: Mutex::new(None), direct_probe: false,
        });
        let kernel = Kernel::new(fixture.kernel_options(), model, gate.clone()).unwrap();
        let session = kernel.create_session(fixture.session_options()).await.unwrap();
        kernel.submit_turn(&session.session_id, "Read the current snapshot".into(), TurnSubmissionOptions::default()).await.unwrap();
        wait_for_kind(&kernel, &session.session_id, "turn_complete").await;
        let original = all_facts(&kernel, &session.session_id).await.into_iter().find(|fact|
            fact["kind"] == "request" && fact["fact"]["attempt"]["execution"] == "completed" && fact["fact"]["request"]["tool_name"].as_str().is_some_and(|name| name.ends_with("public_smoke"))
        ).unwrap()["fact"].clone();
        let oracle = std::fs::read(fixture.0.join("echo-calls.jsonl")).unwrap();
        kernel.shutdown().await.unwrap();
        drop(kernel);
        gate.allowed.store(false, Ordering::SeqCst);
        gate.reads.lock().unwrap().clear();
        let model = Arc::new(ScriptedModel {
            requests: Mutex::new(Vec::new()),
            source: "const smoke=ALL_TOOLS.find(t=>t.name.endsWith('fixture__public_smoke')); try { text(await tools[smoke.name]({})); } catch (error) { text(String(error)); }",
            exec_count: 1, cancel_boundary: None, cancelled_cell: Mutex::new(None), direct_probe: false,
        });
        let rebuilt = Kernel::new(fixture.kernel_options(), model.clone(), gate.clone()).unwrap();
        let resumed = rebuilt.resume_session(PathBuf::from(session.rollout_path.unwrap()), fixture.session_options()).await.unwrap();
        rebuilt.submit_turn(&resumed.session_id, "Read after authorization was revoked".into(), TurnSubmissionOptions::default()).await.unwrap();
        wait_for_kind(&rebuilt, &resumed.session_id, "turn_complete").await;
        let outputs = latest_exec_outputs(&model);
        let current = serde_json::to_string(outputs.last().unwrap()).unwrap();
        assert!(current.contains("shared_result_read_denied"), "{current}");
        assert!(!current.contains("structuredContent"), "a denied read cannot expose the retained body");
        let reads = gate.reads.lock().unwrap().clone();
        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0].request_sequence, original["request_sequence"].as_i64().unwrap());
        assert_eq!(reads[0].attempt_id, original["attempt"]["attempt_id"].as_str().unwrap());
        assert_eq!(std::fs::read(fixture.0.join("echo-calls.jsonl")).unwrap(), oracle, "denied recovery cannot repeat physical execution");
        rebuilt.shutdown().await.unwrap();
    });
    });
}
