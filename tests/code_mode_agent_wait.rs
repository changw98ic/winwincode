//! Two real Core agents call the V1 completion adapter from native Code Mode cells.
use super::*;
use std::collections::BTreeMap;

#[derive(Debug)]
struct CompletionModel {
    cycle: bool,
    requests: Mutex<BTreeMap<String, Vec<Value>>>,
    root: Mutex<Option<String>>,
    foreign_target: Mutex<Option<String>>,
}

impl ModelPort for CompletionModel {
    fn stream(
        &self,
        request: ModelPortRequest,
    ) -> BoxFuture<'static, Result<ModelPortStream, ModelPortFailure>> {
        let payload: Value = serde_json::from_str(&request.payload_json).unwrap();
        let thread = payload["threadId"].as_str().unwrap().to_owned();
        let mut root = self.root.lock().unwrap();
        let root = root.get_or_insert_with(|| thread.clone()).clone();
        let mut requests = self.requests.lock().unwrap();
        let history = requests.entry(thread.clone()).or_default();
        let index = history.len();
        assert_eq!(
            payload["request"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["exec", "wait"]
        );
        let last_output = payload["request"]["input"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|item| {
                matches!(
                    item["type"].as_str(),
                    Some("custom_tool_call_output" | "function_call_output")
                )
            })
            .map(|item| item["output"].to_string())
            .unwrap_or_default();
        history.push(payload);
        assert!(index < 30, "completion wait did not finish: {last_output}");
        let item = if index == 0 {
            let source = if thread == root {
                r"
const spawn = ALL_TOOLS.find(tool => tool.name.endsWith('spawn_agent'));
const wait = ALL_TOOLS.find(tool => tool.name.endsWith('wait_agent'));
if (!spawn || !wait) throw new Error('V1 completion tools missing: ' + JSON.stringify(ALL_TOOLS));
const child = await tools[spawn.name]({message:'completion-fixture-child'});
text(child);
text(await tools[wait.name]({targets:[child.agent_id],timeout_ms:12000}));
text('root-completion-cell-finished');
"
                .to_owned()
            } else if self.cycle {
                let target = self
                    .foreign_target
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| root.clone());
                format!(
                    "const wait = ALL_TOOLS.find(tool => tool.name.endsWith('wait_agent')); text(await tools[wait.name]({{targets:[{target:?}],timeout_ms:12000}})); text('child-completion-cell-finished');"
                )
            } else {
                "text('independent-child-started'); await yield_control(); text('child-completion-cell-finished');".into()
            };
            call_item(
                "exec",
                &format!("completion-exec-{thread}"),
                &json!(format!("// @exec: {{\"yield_time_ms\":1000}}\n{source}")),
            )
        } else if let Some((_, tail)) = last_output.rsplit_once("Script running with cell ID ") {
            let cell = tail
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                .next()
                .unwrap();
            call_item(
                "wait",
                &format!("completion-wait-{index}"),
                &json!({"cell_id":cell,"yield_time_ms":1000}),
            )
        } else {
            json!({"type":"message","role":"assistant","phase":"final_answer",
                "content":[{"type":"output_text","text":"completion fixture finished"}]})
        };
        let frames = [json!({"type":"created"}).to_string(),
            json!({"type":"output_item_done","item":item}).to_string(),
            json!({"type":"completed","responseId":format!("completion-{thread}-{index}"),"endTurn":item["type"] == "message"}).to_string()];
        Box::pin(async move {
            Ok(Box::pin(futures::stream::iter(frames.into_iter().map(Ok))) as ModelPortStream)
        })
    }
}

async fn native_completion_wait(cycle: bool, foreign: bool) {
    let fixture = Fixture::new();
    let catalog_path = fixture.0.join("home/models.json");
    let mut catalog: Value =
        serde_json::from_slice(&std::fs::read(&catalog_path).unwrap()).unwrap();
    catalog["models"][0]["multi_agent_version"] = json!("v1");
    std::fs::write(catalog_path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let config_path = fixture.0.join("home/config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(config_path, format!("{config}\n[agents]\nmax_depth = 3\n")).unwrap();
    let model = Arc::new(CompletionModel {
        cycle,
        requests: Mutex::new(BTreeMap::new()),
        root: Mutex::new(None),
        foreign_target: Mutex::new(None),
    });
    let kernel = Kernel::new(
        fixture.kernel_options(),
        model.clone(),
        Arc::new(RecordingGate::default()),
    )
    .unwrap();
    let session = kernel
        .create_session(fixture.session_options())
        .await
        .unwrap();
    if foreign {
        let other_tree = kernel
            .create_session(fixture.session_options())
            .await
            .unwrap();
        *model.foreign_target.lock().unwrap() = Some(other_tree.session_id);
    }
    kernel
        .submit_turn(
            &session.session_id,
            "Exercise real agent completion waits".into(),
            TurnSubmissionOptions::default(),
        )
        .await
        .unwrap();
    wait_for_kind(&kernel, &session.session_id, "turn_complete").await;
    let threads: Vec<_> = model.requests.lock().unwrap().keys().cloned().collect();
    assert_eq!(threads.len(), 2, "must exercise two actual Core agents");
    let mut facts = Vec::new();
    for thread in &threads {
        facts.extend(all_facts(&kernel, thread).await);
    }
    assert_completion_wait_facts(&facts, cycle, foreign, &model);
    assert_completion_model_evidence(&model, &facts, cycle, foreign);
    kernel.shutdown().await.unwrap();
}

fn assert_completion_wait_facts(
    facts: &[Value],
    cycle: bool,
    foreign: bool,
    model: &CompletionModel,
) {
    let agent_waits: Vec<_> = facts
        .iter()
        .filter(|fact| fact["kind"] == "agent_wait" && fact["fact"]["state"] == "waiting")
        .collect();
    assert_eq!(
        agent_waits.len(),
        if cycle && !foreign { 2 } else { 1 },
        "model outputs: {:?}",
        model
            .requests
            .lock()
            .unwrap()
            .values()
            .flatten()
            .flat_map(|request| request["request"]["input"].as_array().unwrap())
            .filter(|item| matches!(
                item["type"].as_str(),
                Some("custom_tool_call_output" | "function_call_output")
            ))
            .collect::<Vec<_>>()
    );
    for wait in &agent_waits {
        let edge = &wait["fact"]["edge"];
        assert_eq!(edge["source"]["kind"], "cell");
        assert_eq!(edge["targets"][0]["kind"], "thread");
        assert_ne!(edge["source"]["thread_id"], edge["targets"][0]["thread_id"]);
        assert!(
            facts.iter().any(|fact| fact["kind"] == "agent_wait"
                && fact["fact"]["state"] == "settled"
                && fact["fact"]["edge"] == *edge),
            "completion or timeout must settle its original wait"
        );
    }
}

fn assert_completion_model_evidence(
    model: &CompletionModel,
    facts: &[Value],
    cycle: bool,
    foreign: bool,
) {
    let requests = model.requests.lock().unwrap().clone();
    let outputs = requests
        .values()
        .flatten()
        .flat_map(|request| request["request"]["input"].as_array().unwrap())
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("custom_tool_call_output" | "function_call_output")
            )
        })
        .map(|item| item["output"].clone())
        .collect::<Vec<_>>();
    let diagnoses = sharing::diagnostic_payloads(&outputs);
    let cycles: Vec<_> = diagnoses
        .iter()
        .filter(|diagnostic| diagnostic["kind"] == "wait_cycle")
        .collect();
    assert_eq!(
        !cycles.is_empty(),
        cycle && !foreign,
        "only the completion cycle should produce a question"
    );
    for diagnosis in cycles {
        let graph = diagnosis["wait_graph"]
            .as_array()
            .expect("model receives typed wait evidence");
        assert_eq!(graph.len(), 4);
        for edge in graph {
            assert!(edge["deadline_unix_ms"].as_u64().unwrap() > 0);
            assert!(edge["source"]["owner_id"].as_str().is_some());
            assert!(
                facts.iter().any(|fact| fact["kind"] == "request"
                    && fact["fact"]["request_sequence"] == edge["request_sequence"]
                    && fact["fact"]["request"]["logical_id"] == edge["logical_id"]),
                "each graph edge resolves to an original Core request"
            );
        }
    }
    let output_text = serde_json::to_string(&outputs).unwrap();
    assert!(output_text.contains("root-completion-cell-finished"));
    if foreign {
        assert!(
            output_text.contains("not_found"),
            "a live thread in another tree must be rejected"
        );
        let foreign = model.foreign_target.lock().unwrap().clone().unwrap();
        assert!(
            facts
                .iter()
                .filter(|fact| fact["kind"] == "agent_wait" && fact["fact"]["state"] == "waiting")
                .all(|fact| fact["fact"]["edge"]["targets"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|target| target["thread_id"] != foreign)),
            "no untrusted cross-tree edge may be recorded"
        );
    }
    assert!(
        !output_text.contains("Script terminated"),
        "diagnosis cannot terminate active cells"
    );
}

#[test]
fn native_agent_completion_cycle_reaches_model_and_timeout_releases_cells() {
    run_native_test(|runtime| runtime.block_on(native_completion_wait(true, false)));
}

#[test]
fn native_independent_agent_completion_wait_has_no_cycle_diagnosis() {
    run_native_test(|runtime| runtime.block_on(native_completion_wait(false, false)));
}

#[test]
fn native_agent_completion_rejects_a_live_thread_in_another_tree() {
    run_native_test(|runtime| runtime.block_on(native_completion_wait(true, true)));
}
