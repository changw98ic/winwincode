//! Trusted host policy through the real Kernel, Code Mode host and MCP process.
use super::*;
use winwincode_kernel::{
    KernelToolResultReadRequest, ToolCoalescingPermission, ToolDependencySnapshot,
    ToolInputContext, ToolInputProof, ToolInputProofRequest, ToolReusePermission,
};

struct SharingGate {
    workspace: PathBuf,
    flip_on_freeze: std::sync::atomic::AtomicBool,
}
fn version(source: &str) -> Option<String> {
    match source {
        "A" => Some("a".repeat(64)),
        "B" => Some("b".repeat(64)),
        _ => None,
    }
}
impl KernelActionGate for SharingGate {
    fn freeze_tool_input(
        &self,
        context: ToolInputContext,
    ) -> BoxFuture<'static, Option<ToolDependencySnapshot>> {
        let input = (context.mcp_server.as_deref() == Some("fixture")
            && context.request.tool_name.ends_with("public_smoke"))
        .then(|| {
            std::fs::read_to_string(self.workspace.join("source.txt"))
                .ok()
                .and_then(|source| version(&source))
        })
        .flatten();
        if input.is_some() && self.flip_on_freeze.swap(false, Ordering::SeqCst) {
            std::fs::write(self.workspace.join("source.txt"), "B").unwrap();
        }
        Box::pin(async move {
            input.map(|dependency_digest| ToolDependencySnapshot {
                policy_revision: "1".repeat(64),
                dependency_digest,
                account_scope_digest: "2".repeat(64),
                session_scope_digest: "3".repeat(64),
                validity_epoch: "4".repeat(64),
                reuse: ToolReusePermission::ImmutableValue,
                coalescing: ToolCoalescingPermission::SharedRead,
            })
        })
    }
    fn verify_tool_input(
        &self,
        request: ToolInputProofRequest,
    ) -> BoxFuture<'static, Option<ToolInputProof>> {
        let source = request.output["structuredContent"]["source"]
            .as_str()
            .and_then(version);
        assert!(
            source.is_some(),
            "missing verified source: {}",
            request.output
        );
        Box::pin(async move {
            source.map(|input_digest| ToolInputProof {
                evidence_digest: input_digest.clone(),
                input_digest,
            })
        })
    }
    fn authorize_result_read(
        &self,
        request: KernelToolResultReadRequest,
    ) -> BoxFuture<'static, Result<KernelActionAuthorization, KernelFailure>> {
        Box::pin(async move { Ok(KernelActionAuthorization::new(request.operation_id, None)) })
    }
    fn revalidate_result_read(
        &self,
        request: KernelToolResultReadRequest,
        authorization: KernelActionAuthorization,
    ) -> BoxFuture<'static, Result<(), KernelFailure>> {
        Box::pin(async move {
            if request.operation_id == authorization.request_binding() {
                Ok(())
            } else {
                Err(KernelFailure::action_rejected())
            }
        })
    }
    fn authorize(
        &self,
        request: KernelActionRequest,
    ) -> BoxFuture<'static, Result<KernelActionAuthorization, KernelFailure>> {
        Box::pin(async move { Ok(KernelActionAuthorization::new(request.operation_id, None)) })
    }
    fn revalidate(
        &self,
        request: KernelActionRequest,
        authorization: KernelActionAuthorization,
    ) -> BoxFuture<'static, Result<(), KernelFailure>> {
        Box::pin(async move {
            if request.operation_id == authorization.request_binding() {
                Ok(())
            } else {
                Err(KernelFailure::action_rejected())
            }
        })
    }
}
const SHARING_SOURCE: &str = r"
const smoke = ALL_TOOLS.find(tool => tool.name.endsWith('fixture__public_smoke'));
const concurrent = await Promise.all([tools[smoke.name]({}), tools[smoke.name]({}), tools[smoke.name]({})]);
for (const result of concurrent) text(result);
for (let i = 0; i < 6; i++) text(await tools[smoke.name]({}));
await yield_control();
text('shared-cell-resumed');
";

#[test]
fn native_progress_write_failure_preserves_accepted_tool_results() {
    run_native_test(|runtime| {
        runtime.block_on(async {
            let fixture = Fixture::new();
            std::fs::write(fixture.0.join("workspace/source.txt"), "A").unwrap();
            let model = Arc::new(ScriptedModel {
                requests: Mutex::new(Vec::new()), source: SHARING_SOURCE, exec_count: 1,
                cancel_boundary: None, cancelled_cell: Mutex::new(None), direct_probe: false,
            });
            let gate = Arc::new(SharingGate {
                workspace: fixture.0.join("workspace"),
                flip_on_freeze: std::sync::atomic::AtomicBool::new(false),
            });
            let kernel = Kernel::new(fixture.kernel_options(), model.clone(), gate).unwrap();
            let session = kernel.create_session(fixture.session_options()).await.unwrap();
            let database = std::fs::read_dir(fixture.0.join("home")).unwrap()
                .filter_map(Result::ok).map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|extension| extension == "sqlite"))
                .find_map(|path| {
                    let database = rusqlite::Connection::open(path).unwrap();
                    database.query_row("SELECT name FROM sqlite_master WHERE name='tool_progress_receipts'",
                        [], |row| row.get::<_, String>(0)).ok().map(|_| database)
                }).expect("Core state database");
            database.execute_batch("CREATE TRIGGER fail_diagnostic_progress BEFORE INSERT ON tool_progress_receipts BEGIN SELECT RAISE(FAIL, 'progress fixture failure'); END;").unwrap();
            kernel.submit_turn(&session.session_id, "Read the trusted source".into(),
                TurnSubmissionOptions::default()).await.unwrap();
            wait_for_kind(&kernel, &session.session_id, "turn_complete").await;
            let outputs = serde_json::to_string(&latest_exec_outputs(&model)).unwrap();
            assert!(outputs.contains("shared-cell-resumed"), "{outputs}");
            assert!(!outputs.contains("progress fixture failure"), "{outputs}");
            assert_smoke_outputs(&model, &vec![json!({"source":"A","execution":1}); 9]);
            let facts = all_facts(&kernel, &session.session_id).await;
            assert_eq!(latest_exec_requests(&facts).len(), 9);
            assert_model_diagnostic_sources(&model, &facts);
            assert_eq!(facts.iter().filter(|fact| fact["kind"] == "progress").count(), 0);
            let accepted = facts.iter().filter(|fact| fact["kind"] == "request"
                && fact["fact"]["request"]["tool_name"].as_str().is_some_and(|name| name.ends_with("public_smoke"))
                && fact["fact"]["attempt"]["disposition"] == "accepted"
                && fact["fact"]["attempt"]["delivery"] == "offered").count();
            assert!(accepted > 0, "accepted execution must still be offered");
            assert_model_receipts(&model, &facts);
            kernel.shutdown().await.unwrap();
        });
    });
}
#[derive(Debug)]
struct DiagnosticOfferFaultModel {
    script: ScriptedModel,
    output_seen: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

impl ModelPort for DiagnosticOfferFaultModel {
    fn stream(
        &self,
        request: ModelPortRequest,
    ) -> BoxFuture<'static, Result<ModelPortStream, ModelPortFailure>> {
        let first_exec_output = self.script.requests.lock().unwrap().len() == 1;
        let stream = self.script.stream(request);
        if !first_exec_output {
            return stream;
        }
        let output_seen = Arc::clone(&self.output_seen);
        let resume = Arc::clone(&self.resume);
        Box::pin(async move {
            output_seen.notify_one();
            resume.notified().await;
            stream.await
        })
    }
}

fn diagnostic_payloads(outputs: &[Value]) -> Vec<Value> {
    fn collect(value: &Value, diagnostics: &mut Vec<Value>) {
        match value {
            Value::String(text) => {
                if let Some((_, tail)) = text.split_once("<model_behavior_diagnosis>") {
                    let body = tail.split_once("</model_behavior_diagnosis>").unwrap().0;
                    let fragment: Value = serde_json::from_str(body).unwrap();
                    assert_eq!(fragment["type"], "model_behavior_diagnosis");
                    diagnostics.extend(fragment["diagnostics"].as_array().unwrap().iter().cloned());
                }
            }
            Value::Array(values) => values.iter().for_each(|value| collect(value, diagnostics)),
            Value::Object(values) => values
                .values()
                .for_each(|value| collect(value, diagnostics)),
            _ => {}
        }
    }
    let mut diagnostics = Vec::new();
    outputs
        .iter()
        .for_each(|output| collect(output, &mut diagnostics));
    diagnostics
}

#[test]
fn native_diagnostic_offer_rollback_preserves_exec_and_retries_on_same_cell_wait() {
    run_native_test(|runtime| {
        runtime.block_on(async {
            let fixture = Fixture::new();
            std::fs::write(fixture.0.join("workspace/source.txt"), "A").unwrap();
            let model = Arc::new(DiagnosticOfferFaultModel {
                script: ScriptedModel {
                    requests: Mutex::new(Vec::new()),
                    source: SHARING_SOURCE,
                    exec_count: 1,
                    cancel_boundary: None,
                    cancelled_cell: Mutex::new(None),
                    direct_probe: false,
                },
                output_seen: Arc::new(tokio::sync::Notify::new()),
                resume: Arc::new(tokio::sync::Notify::new()),
            });
            let gate = Arc::new(SharingGate {
                workspace: fixture.0.join("workspace"),
                flip_on_freeze: std::sync::atomic::AtomicBool::new(false),
            });
            let kernel = Kernel::new(fixture.kernel_options(), model.clone(), gate).unwrap();
            let session = kernel.create_session(fixture.session_options()).await.unwrap();
            let database = std::fs::read_dir(fixture.0.join("home")).unwrap()
                .filter_map(Result::ok).map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|extension| extension == "sqlite"))
                .find_map(|path| {
                    let database = rusqlite::Connection::open(path).unwrap();
                    database.query_row("SELECT name FROM sqlite_master WHERE name='tool_diagnostic_feedback'",
                        [], |row| row.get::<_, String>(0)).ok().map(|_| database)
                }).expect("Core state database");
            // This is a deterministic whole-transaction rollback, not SQLITE_FULL.
            database.execute_batch("CREATE TRIGGER fail_optional_diagnostic_offer BEFORE INSERT ON tool_fact_events WHEN json_extract(NEW.fact_json, '$.kind') = 'diagnostic' AND json_extract(NEW.fact_json, '$.fact.delivery') = 'offered' BEGIN SELECT RAISE(ROLLBACK, 'optional diagnostic offer fixture'); END;").unwrap();
            kernel.submit_turn(&session.session_id, "Inspect shared reads while diagnostic delivery fails".into(),
                TurnSubmissionOptions::default()).await.unwrap();
            tokio::time::timeout(Duration::from_secs(30), model.output_seen.notified())
                .await.expect("model received current exec output under diagnostic rollback");

            // Observe the fault before any wait can offer a later diagnostic version.
            let outputs = latest_exec_outputs(&model.script);
            assert_eq!(outputs.len(), 1, "only the current exec has returned");
            let current_output = serde_json::to_string(&outputs[0]).unwrap();
            let (_, rest) = current_output.split_once("Script running with cell ID ")
                .unwrap_or_else(|| panic!("optional diagnostic failure must preserve the yielded cell identity; current output: {current_output}"));
            let cell_id = rest.split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                .next().unwrap().to_owned();
            assert!(!current_output.contains("optional diagnostic offer fixture"));
            assert_smoke_outputs(&model.script, &vec![json!({"source":"A","execution":1}); 9]);
            let before = all_facts(&kernel, &session.session_id).await;
            let exec = before.iter().rfind(|fact| fact["kind"] == "request"
                && matches!(fact["fact"]["request"]["tool_name"].as_str(), Some("exec" | "functions.exec")))
                .expect("current exec receipt");
            let exec_sequence = exec["fact"]["request_sequence"].as_i64().unwrap();
            assert_eq!(exec["fact"]["attempt"]["disposition"], "accepted");
            assert_eq!(exec["fact"]["attempt"]["delivery"], "offered");
            let cell = before.iter().rfind(|fact| fact["kind"] == "cell"
                && fact["fact"]["parent_request_sequence"] == exec_sequence)
                .expect("current exec cell");
            assert_eq!(cell["fact"]["cell_id"], cell_id);
            assert_eq!(cell["fact"]["lifecycle"], "live");
            let nested = latest_exec_requests(&before);
            assert_eq!(nested.len(), 9);
            assert!(nested.values().all(|request| request["request"]["cell_id"] == cell_id
                && request["request"]["scope_id"] == cell["fact"]["scope_id"]));
            assert_eq!(nested.values().filter(|request| !request["attempt"].is_null()).count(), 1);
            for (sequence, request) in &nested {
                let receipt = if request["attempt"].is_null() {
                    &before.iter().rfind(|fact| fact["kind"] == "sharing"
                        && fact["fact"]["request_sequence"] == *sequence).expect("logical shared receipt")["fact"]
                } else {
                    &request["attempt"]
                };
                assert_eq!(receipt["disposition"], "accepted");
                assert_eq!(receipt["delivery"], "offered");
            }
            assert_model_receipts(&model.script, &before);
            let diagnostics = diagnostic_payloads(&outputs);
            assert!(!diagnostics.is_empty(), "exec must still expose its staged question");
            for diagnostic in &diagnostics {
                let same_version = |fact: &&Value| fact["kind"] == "diagnostic"
                    && fact["fact"]["diagnostic"]["diagnostic_id"] == diagnostic["diagnostic_id"]
                    && fact["fact"]["diagnostic"]["evidence_version"] == diagnostic["evidence_version"];
                let queued = before.iter().filter(same_version)
                    .find(|fact| fact["fact"]["delivery"] == "queued").expect("question remains durably queued");
                assert!(before.iter().filter(same_version).all(|fact| fact["fact"]["delivery"] != "offered"));
                assert!(diagnostic["calls"].as_array().unwrap().iter().all(|call|
                    nested.contains_key(&call["request_sequence"].as_i64().unwrap())));
                let (encoded, offered): (String, i64) = database.query_row(
                    "SELECT diagnostic_json, offered FROM tool_diagnostic_feedback WHERE boundary_request_sequence = ?1 AND diagnostic_id = ?2 AND evidence_version = ?3",
                    rusqlite::params![exec_sequence, diagnostic["diagnostic_id"].as_str().unwrap(), diagnostic["evidence_version"].as_i64().unwrap()],
                    |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
                assert_eq!(offered, 0, "rolled-back diagnostic delivery stays retryable");
                let offered_version: i64 = database.query_row(
                    "SELECT offered_version FROM tool_diagnostics WHERE diagnostic_id = ?1",
                    [diagnostic["diagnostic_id"].as_str().unwrap()], |row| row.get(0)).unwrap();
                assert_eq!(offered_version, 0, "rollback must not advance diagnostic delivery");
                assert_eq!(serde_json::from_str::<Value>(&encoded).unwrap(), queued["fact"]["diagnostic"]);
            }

            database.execute_batch("DROP TRIGGER fail_optional_diagnostic_offer;").unwrap();
            model.resume.notify_one();
            wait_for_kind(&kernel, &session.session_id, "turn_complete").await;
            let current_outputs = latest_exec_outputs(&model.script);
            let history = serde_json::to_string(&current_outputs).unwrap();
            assert!(current_outputs.len() >= 2, "current exec must have a wait output");
            assert!(current_outputs.last().unwrap().to_string().contains("shared-cell-resumed"),
                "the current wait output must read the same cell's completion");
            assert!(!history.contains("code_cell_recovery"), "necessary offer must preserve usable cell ownership");
            assert_smoke_outputs(&model.script, &vec![json!({"source":"A","execution":1}); 9]);
            let after = all_facts(&kernel, &session.session_id).await;
            let waits: std::collections::BTreeMap<_, _> = after.iter()
                .filter(|fact| fact["kind"] == "request" && matches!(fact["fact"]["request"]["tool_name"].as_str(), Some("wait" | "functions.wait")))
                .map(|fact| (fact["fact"]["request_sequence"].as_i64().unwrap(), &fact["fact"]))
                .collect();
            assert!(!waits.is_empty(), "yield must be followed by a real wait");
            assert!(waits.values().all(|wait| wait["attempt"]["disposition"] == "accepted" && wait["attempt"]["delivery"] == "offered"));
            for sequence in waits.keys() {
                assert!(after.iter().any(|fact| fact["kind"] == "wait"
                    && fact["fact"]["waiter_request_sequence"] == *sequence
                    && fact["fact"]["target_cell_sequence"] == cell["fact"]["sequence"]
                    && fact["fact"]["state"] == "settled"));
            }
            assert!(after.iter().any(|fact| fact["kind"] == "cell"
                && fact["fact"]["cell_id"] == cell_id
                && fact["fact"]["scope_id"] == cell["fact"]["scope_id"]
                && fact["fact"]["parent_request_sequence"] == exec_sequence
                && fact["fact"]["lifecycle"] == "closed"));
            assert_eq!(after.iter().filter(|fact| fact["kind"] == "request"
                && matches!(fact["fact"]["request"]["tool_name"].as_str(), Some("exec" | "functions.exec")))
                .map(|fact| fact["fact"]["request_sequence"].as_i64().unwrap())
                .collect::<std::collections::BTreeSet<_>>().len(), 1);
            let nested_after = latest_exec_requests(&after);
            assert_eq!(nested_after.keys().copied().collect::<Vec<_>>(), nested.keys().copied().collect::<Vec<_>>());
            assert_eq!(nested_after.values().filter(|request| !request["attempt"].is_null()).count(), 1);
            for diagnostic in &diagnostics {
                assert!(after.iter().any(|fact| fact["kind"] == "diagnostic"
                    && fact["fact"]["delivery"] == "offered"
                    && fact["fact"]["diagnostic"]["diagnostic_id"] == diagnostic["diagnostic_id"]
                    && fact["fact"]["diagnostic"]["evidence_version"] == diagnostic["evidence_version"]
                    && waits.contains_key(&fact["fact"]["boundary_request_sequence"].as_i64().unwrap())),
                    "the queued question must retry at an actual wait boundary");
            }
            assert_model_diagnostic_sources(&model.script, &after);
            assert_model_receipts(&model.script, &after);
            kernel.shutdown().await.unwrap();
        });
    });
}

#[test]
fn native_shared_execution_reuse_and_diagnosis_keep_each_logical_observation() {
    run_native_test(|runtime| {
        runtime.block_on(async {
            let fixture = Fixture::new();
            std::fs::write(fixture.0.join("workspace/source.txt"), "A").unwrap();
            let model = Arc::new(ScriptedModel {
                requests: Mutex::new(Vec::new()),
                source: SHARING_SOURCE,
                exec_count: 1,
                cancel_boundary: None,
                cancelled_cell: Mutex::new(None),
                direct_probe: false,
            });
            let gate = Arc::new(SharingGate {
                workspace: fixture.0.join("workspace"),
                flip_on_freeze: std::sync::atomic::AtomicBool::new(false),
            });
            let kernel = Kernel::new(fixture.kernel_options(), model.clone(), gate).unwrap();
            let session = kernel
                .create_session(fixture.session_options())
                .await
                .unwrap();
            for (source, execution) in [("A", 1), ("B", 2), ("A", 1)] {
                std::fs::write(fixture.0.join("workspace/source.txt"), source).unwrap();
                model.requests.lock().unwrap().clear();
                kernel
                    .submit_turn(
                        &session.session_id,
                        "Inspect trusted shared reads".into(),
                        TurnSubmissionOptions::default(),
                    )
                    .await
                    .unwrap();
                wait_for_kind(&kernel, &session.session_id, "turn_complete").await;
                let history = serde_json::to_string(&latest_exec_outputs(&model)).unwrap();
                assert!(
                    history.contains("shared-cell-resumed"),
                    "cell did not resume"
                );
                assert!(
                    history.contains("repeated_operation"),
                    "model did not receive diagnosis"
                );
                assert!(
                    !history.contains("tool_sharing:"),
                    "shared operation failed"
                );
                assert_smoke_outputs(
                    &model,
                    &vec![json!({"source":source,"execution":execution}); 9],
                );
                let facts = all_facts(&kernel, &session.session_id).await;
                assert_eq!(latest_exec_requests(&facts).len(), 9);
                assert_model_diagnostic_sources(&model, &facts);
                assert_model_receipts(&model, &facts);
            }
            let facts = all_facts(&kernel, &session.session_id).await;
            let requests: std::collections::BTreeMap<_, _> = facts
                .iter()
                .filter(|f| {
                    f["kind"] == "request"
                        && f["fact"]["request"]["tool_name"]
                            .as_str()
                            .is_some_and(|t| t.ends_with("public_smoke"))
                })
                .map(|f| (f["fact"]["request_sequence"].as_i64().unwrap(), &f["fact"]))
                .collect();
            assert_eq!(requests.len(), 27);
            assert_eq!(
                requests
                    .values()
                    .filter(|r| !r["attempt"].is_null())
                    .count(),
                2
            );
            let shares: std::collections::BTreeMap<_, _> = facts
                .iter()
                .filter(|f| f["kind"] == "sharing")
                .map(|f| (f["fact"]["request_sequence"].as_i64().unwrap(), &f["fact"]))
                .collect();
            assert_eq!(shares.len(), 25);
            assert_eq!(shares.values().filter(|s| s["kind"] == "merged").count(), 4);
            assert_eq!(shares.values().filter(|s| s["kind"] == "reuse").count(), 21);
            assert!(
                shares
                    .values()
                    .all(|s| s["disposition"] == "accepted" && s["delivery"] == "offered")
            );
            assert_eq!(facts.iter().filter(|f| f["kind"] == "progress").count(), 2);
            kernel.shutdown().await.unwrap();
        });
    });
}

const INPUT_CHANGE_SOURCE: &str = r"
const smoke = ALL_TOOLS.find(tool => tool.name.endsWith('fixture__public_smoke'));
text(await tools[smoke.name]({}));
const concurrent = await Promise.all([tools[smoke.name]({}), tools[smoke.name]({}), tools[smoke.name]({})]);
for (const result of concurrent) text(result);
for (let i = 0; i < 6; i++) text(await tools[smoke.name]({}));
await yield_control();
text('shared-cell-resumed');
";
#[test]
fn native_input_change_during_execution_is_observed_and_requires_new_validation() {
    run_native_test(|runtime| {
        runtime.block_on(async {
            let fixture = Fixture::new();
            std::fs::write(fixture.0.join("workspace/source.txt"), "A").unwrap();
            let model = Arc::new(ScriptedModel {
                requests: Mutex::new(Vec::new()),
                source: INPUT_CHANGE_SOURCE,
                exec_count: 1,
                cancel_boundary: None,
                cancelled_cell: Mutex::new(None),
                direct_probe: false,
            });
            let gate = Arc::new(SharingGate {
                workspace: fixture.0.join("workspace"),
                flip_on_freeze: std::sync::atomic::AtomicBool::new(true),
            });
            let kernel = Kernel::new(fixture.kernel_options(), model.clone(), gate).unwrap();
            let session = kernel
                .create_session(fixture.session_options())
                .await
                .unwrap();
            kernel
                .submit_turn(
                    &session.session_id,
                    "Validate the source consumed by execution".into(),
                    TurnSubmissionOptions::default(),
                )
                .await
                .unwrap();
            wait_for_kind(&kernel, &session.session_id, "turn_complete").await;
            let history = serde_json::to_string(&latest_exec_outputs(&model)).unwrap();
            assert!(history.contains("shared-cell-resumed"));
            assert!(
                !history.contains("tool_sharing:"),
                "tool outputs: {history}"
            );
            let mut expected = vec![json!({"source":"B","execution":1})];
            expected.extend(vec![json!({"source":"B","execution":2}); 9]);
            assert_smoke_outputs(&model, &expected);
            let facts = all_facts(&kernel, &session.session_id).await;
            assert_eq!(latest_exec_requests(&facts).len(), 10);
            assert_model_diagnostic_sources(&model, &facts);
            assert_model_receipts(&model, &facts);
            let mut validations: Vec<_> = facts
                .iter()
                .filter(|f| f["kind"] == "input_validation")
                .map(|f| f["fact"]["validation"].as_str().unwrap())
                .collect();
            validations.sort_unstable();
            assert_eq!(validations, ["mismatch", "verified"]);
            assert_eq!(facts.iter().filter(|f| f["kind"] == "progress").count(), 1);
            let source = facts
                .iter()
                .find(|f| f["kind"] == "input_validation" && f["fact"]["validation"] == "mismatch")
                .unwrap()["fact"]["request_sequence"]
                .as_i64()
                .unwrap();
            assert!(
                facts
                    .iter()
                    .filter(|f| f["kind"] == "sharing")
                    .all(|f| f["fact"]["source_request_sequence"] != source)
            );
            kernel.shutdown().await.unwrap();
        });
    });
}

fn assert_smoke_outputs(model: &ScriptedModel, expected: &[Value]) {
    fn collect(value: &Value, reports: &mut Vec<Value>) {
        match value {
            Value::String(text) => {
                if let Ok(result) = serde_json::from_str::<Value>(text)
                    && let Some(report) = result.get("structuredContent")
                {
                    let content = result["content"][0]["text"]
                        .as_str()
                        .expect("native MCP report text");
                    assert_eq!(serde_json::from_str::<Value>(content).unwrap(), *report);
                    reports.push(report.clone());
                }
            }
            Value::Array(values) => values.iter().for_each(|value| collect(value, reports)),
            Value::Object(values) => values.values().for_each(|value| collect(value, reports)),
            _ => {}
        }
    }
    let mut reports = Vec::new();
    for output in latest_exec_outputs(model) {
        collect(&output, &mut reports);
    }
    assert_eq!(reports.as_slice(), expected);
}

fn model_receipts(model: &ScriptedModel) -> Vec<Vec<Value>> {
    fn collect(value: &Value, receipts: &mut Vec<Vec<Value>>) {
        match value {
            Value::String(text) => {
                if let Some((_, tail)) = text.split_once("<core_tool_receipts>") {
                    let body = tail.split_once("</core_tool_receipts>").unwrap().0;
                    let fragment: Value = serde_json::from_str(body).unwrap();
                    receipts.push(fragment["receipts"].as_array().unwrap().clone());
                }
            }
            Value::Array(values) => values.iter().for_each(|value| collect(value, receipts)),
            Value::Object(values) => values.values().for_each(|value| collect(value, receipts)),
            _ => {}
        }
    }
    let mut receipts = Vec::new();
    for output in latest_exec_outputs(model) {
        collect(&output, &mut receipts);
    }
    receipts
}

fn assert_model_receipts(model: &ScriptedModel, facts: &[Value]) {
    let batches = model_receipts(model);
    assert!(!batches.is_empty(), "latest exec received no Core receipts");
    let requests = latest_exec_requests(facts);
    // Core projects the latest four references at each boundary. These scripts
    // execute no nested calls after yielding, so exec and wait expose the same four.
    let expected: Vec<_> = requests.keys().rev().take(4).copied().collect();
    for receipts in batches {
        let sequences = receipts
            .iter()
            .map(|receipt| receipt["request_sequence"].as_i64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            sequences, expected,
            "latest exec must expose its complete ordered latest-four receipt projection"
        );
        for receipt in receipts {
            let sequence = receipt["request_sequence"].as_i64().unwrap();
            let request = requests[&sequence];
            assert_eq!(receipt["source_id"], request["request"]["logical_id"]);
            assert_eq!(receipt["tool"], request["request"]["tool_name"]);
            assert_eq!(receipt["disposition"], "accepted");
            assert_eq!(receipt["delivery"], "offered");
            if let Some(shared) = facts.iter().rfind(|fact| {
                fact["kind"] == "sharing" && fact["fact"]["request_sequence"] == sequence
            }) {
                assert_eq!(
                    receipt["source_request_sequence"],
                    shared["fact"]["source_request_sequence"]
                );
                assert!(
                    receipt["execution"].is_null(),
                    "logical sharing invented an attempt"
                );
            } else {
                assert!(receipt["source_request_sequence"].is_null());
                assert_eq!(receipt["execution"], request["attempt"]["execution"]);
            }
        }
    }
}

#[test]
#[should_panic(expected = "latest exec received no Core receipts")]
fn historical_receipts_cannot_substitute_for_latest_exec_feedback() {
    let (model, facts) = historical_feedback_fixture();
    assert!(model_outputs(&model).contains("core_tool_receipts"));
    assert_model_receipts(&model, &facts);
}

#[test]
#[should_panic(expected = "complete ordered latest-four receipt projection")]
fn republished_historical_receipts_cannot_substitute_for_current_scope_receipts() {
    let (model, facts) = historical_feedback_fixture();
    {
        let mut requests = model.requests.lock().unwrap();
        let input = requests[0]["request"]["input"].as_array_mut().unwrap();
        input[3]["output"] = input[1]["output"].clone();
    }
    assert_model_receipts(&model, &facts);
}
