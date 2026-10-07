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
            let outputs = model_outputs(&model);
            assert!(outputs.contains("shared-cell-resumed"), "{outputs}");
            assert!(!outputs.contains("progress fixture failure"), "{outputs}");
            let facts = all_facts(&kernel, &session.session_id).await;
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
            for source in ["A", "B", "A"] {
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
                let history = model_outputs(&model);
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
            assert_model_receipts(&model, &facts);
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
            let history = model_outputs(&model);
            assert!(history.contains("shared-cell-resumed"));
            assert!(
                !history.contains("tool_sharing:"),
                "tool outputs: {history}"
            );
            let facts = all_facts(&kernel, &session.session_id).await;
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

async fn all_facts(kernel: &Kernel, session: &str) -> Vec<Value> {
    let mut cursor = 0;
    let mut facts = Vec::new();
    loop {
        let page = kernel
            .tool_runtime_events(session, cursor, 200)
            .await
            .unwrap();
        if page.is_empty() {
            return facts;
        }
        cursor = page.last().unwrap().source_sequence;
        facts.extend(
            page.into_iter()
                .map(|event| serde_json::from_str(&event.fact_json).unwrap()),
        );
    }
}

fn model_outputs(model: &ScriptedModel) -> String {
    model
        .requests
        .lock()
        .unwrap()
        .iter()
        .flat_map(|request| request["request"]["input"].as_array().unwrap())
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("custom_tool_call_output" | "function_call_output")
            )
        })
        .map(|item| item["output"].to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn model_receipts(model: &ScriptedModel) -> Vec<Value> {
    fn collect(value: &Value, receipts: &mut Vec<Value>) {
        match value {
            Value::String(text) => {
                if let Some((_, tail)) = text.split_once("<core_tool_receipts>") {
                    let body = tail.split_once("</core_tool_receipts>").unwrap().0;
                    let fragment: Value = serde_json::from_str(body).unwrap();
                    receipts.extend(fragment["receipts"].as_array().unwrap().iter().cloned());
                }
            }
            Value::Array(values) => values.iter().for_each(|value| collect(value, receipts)),
            Value::Object(values) => values.values().for_each(|value| collect(value, receipts)),
            _ => {}
        }
    }
    let mut receipts = Vec::new();
    for request in model.requests.lock().unwrap().iter() {
        for item in request["request"]["input"].as_array().unwrap() {
            if matches!(
                item["type"].as_str(),
                Some("custom_tool_call_output" | "function_call_output")
            ) {
                collect(&item["output"], &mut receipts);
            }
        }
    }
    receipts
}

fn assert_model_receipts(model: &ScriptedModel, facts: &[Value]) {
    let receipts = model_receipts(model);
    assert!(
        !receipts.is_empty(),
        "model received no original Core references"
    );
    for receipt in receipts {
        let sequence = &receipt["request_sequence"];
        let request = facts
            .iter()
            .find(|fact| fact["kind"] == "request" && fact["fact"]["request_sequence"] == *sequence)
            .unwrap();
        assert_eq!(
            receipt["source_id"],
            request["fact"]["request"]["logical_id"]
        );
        assert_eq!(receipt["tool"], request["fact"]["request"]["tool_name"]);
        assert_eq!(receipt["disposition"], "accepted");
        assert_eq!(receipt["delivery"], "offered");
        if let Some(shared) = facts
            .iter()
            .find(|fact| fact["kind"] == "sharing" && fact["fact"]["request_sequence"] == *sequence)
        {
            assert_eq!(
                receipt["source_request_sequence"],
                shared["fact"]["source_request_sequence"]
            );
            assert!(
                receipt["execution"].is_null(),
                "logical sharing invented an attempt"
            );
        }
    }
}
