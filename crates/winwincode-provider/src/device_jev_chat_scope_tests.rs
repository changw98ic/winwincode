// SPDX-License-Identifier: Apache-2.0

//! Ordinary Chat has its own sealed task scope; Delivery keeps its exact plan gate.

use super::*;
use winwincode_execution_port::{
    agent_config::{AgentProfileSettings, resolve_agent_session_config},
    generated::{
        ExecutionJob, ExecutionScope, ModelOpenMessage, ProductSessionExecutionScope,
        ProductSessionExecutionScopeKind,
    },
};

fn fixture() -> (ModelOpenMessage, ExecutionJob) {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let messages = fixture["messages"].as_array().unwrap();
    let open = serde_json::from_value(
        messages
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let mut job: ExecutionJob = serde_json::from_value(
        messages
            .iter()
            .find(|message| message["kind"] == "job.dispatch")
            .unwrap()["job"]
            .clone(),
    )
    .unwrap();
    let input = job.work_input.as_mut().unwrap();
    input.work_plan = Some(vec![input.work_item.clone()]);
    (open, job)
}

fn store_and_request(job: &ExecutionJob, label: &str) -> (DeviceProviderStore, serde_json::Value) {
    let root =
        std::env::temp_dir().join(format!("wwc-jev-chat-scope-{label}-{}", std::process::id()));
    let store = DeviceProviderStore::open(&root).unwrap();
    let settings: crate::OpenJevRemoteSettings = serde_json::from_value(serde_json::json!({
        "providerId":"fixture-jev-chat", "endpoint":"https://nli.invalid/score", "apiKey":"offline-only",
        "modelId":"fixture-jev-model", "timeoutMs":1000,"retries":0,"devices":["remote"],"dtypes":["auto"]
    })).unwrap();
    store.save_jev_settings(&settings).unwrap();
    let profile: AgentProfileSettings = serde_json::from_value(serde_json::json!({
        "provider":"fixture","model":"fixture-model","reasoning":"max","tools":[],"sandbox":"candidate","instructions":null,
        "jevContext":{"provider":"fixture-jev-chat","policy":{"version":"fixture-policy","minimumConfidence":0.6,
          "pinThreshold":0.9,"keepThreshold":0.9,"compactThreshold":0.9,"dropThreshold":0.9,
          "taskMemoryThreshold":0.5,"projectMemoryThreshold":0.7,"longTermMemoryThreshold":0.9}}
    })).unwrap();
    let session = resolve_agent_session_config(
        &winwincode_domain::WorkerId("worker-fixture".into()),
        &serde_json::from_value(
            serde_json::json!({"capabilityDigest":format!("sha256:{}", "a".repeat(64)),
          "features":[],"maxConcurrentJobs":1,"platform":"aarch64-apple-darwin"}),
        )
        .unwrap(),
        "chat",
        profile,
    )
    .unwrap();
    let request = serde_json::json!({"provider":"fixture","winwincodeJevContext":session,
    "winwincodeJevTask":{"job":job,"jobDigest":format!("sha256:{:x}",Sha256::digest(serde_json::to_vec(job).unwrap()))},
    "request":{"model":"fixture-model","instructions":"Original Chat instructions","input":[
      {"type":"message","role":"system","content":"Protect the original instruction"},
      {"type":"message","role":"user","content":"Check the original goal"},
      {"type":"custom_tool_call","name":"exec","call_id":"tool","input":"text('fact')"},
      {"type":"custom_tool_call_output","call_id":"tool","output":"fact"},
      {"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Current progress"}]}
    ]}});
    (store, request)
}

fn finish(store: DeviceProviderStore) {
    let root = std::path::Path::new(store.connection.path().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM jev_context_exchanges", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM jev_judge_exchanges", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn ordinary_chat_preparation_preserves_its_original_scope_instructions_and_tool_facts() {
    let (mut open, mut job) = fixture();
    job.scope = ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
        kind: ProductSessionExecutionScopeKind::ProductSession,
        product_session_id: open.session_identity.product_session_id.clone(),
    });
    job.work_input = None;
    open.session_identity.work_run_id = None;
    let (store, original) = store_and_request(&job, "ordinary");
    let premise: serde_json::Value =
        serde_json::from_str(&context_task_text(&job, &original).unwrap()).unwrap();
    assert_eq!(premise["goal"], job.goal);
    assert!(premise["workInput"].is_null());
    assert_eq!(premise["instructions"], original["request"]["instructions"]);
    assert_eq!(
        premise["toolRecords"],
        serde_json::json!([
            original["request"]["input"][2],
            original["request"]["input"][3]
        ])
    );
    let mut expected = original.clone();
    expected
        .as_object_mut()
        .unwrap()
        .remove("winwincodeJevContext");
    expected
        .as_object_mut()
        .unwrap()
        .remove("winwincodeJevTask");
    let mut actual = original;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(store.prepare_jev_model_request(&open, &mut actual, &|| true))
        .unwrap();
    assert_eq!(actual, expected);
    finish(store);
}

#[test]
fn jev_preparation_rejects_changed_seals_and_invalid_chat_or_workrun_inputs_before_paid_work() {
    let (open, job) = fixture();
    for case in [
        "workrun-without-input",
        "workrun-without-plan",
        "workrun-with-changed-node",
        "chat-with-work-input",
        "changed-job-digest",
        "changed-provider",
        "changed-model",
    ] {
        let mut changed = job.clone();
        match case {
            "workrun-without-input" => changed.work_input = None,
            "workrun-without-plan" => changed.work_input.as_mut().unwrap().work_plan = None,
            "workrun-with-changed-node" => changed
                .work_input
                .as_mut()
                .unwrap()
                .work_plan
                .as_mut()
                .unwrap()[0]
                .goal
                .push_str(" changed"),
            "chat-with-work-input" => {
                changed.scope =
                    ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
                        kind: ProductSessionExecutionScopeKind::ProductSession,
                        product_session_id: open.session_identity.product_session_id.clone(),
                    });
            }
            _ => {}
        }
        let (store, mut request) = store_and_request(&changed, case);
        match case {
            "changed-job-digest" => {
                request["winwincodeJevTask"]["job"]["goal"] =
                    serde_json::json!("changed without the sealed digest");
            }
            "changed-provider" => request["provider"] = serde_json::json!("another-provider"),
            "changed-model" => request["request"]["model"] = serde_json::json!("another-model"),
            _ => {}
        }
        let original = request.clone();
        assert_eq!(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(store.prepare_jev_model_request(&open, &mut request, &|| true))
                .unwrap_err(),
            DeviceProviderError::StateInvalid,
            "{case}"
        );
        assert_eq!(
            request, original,
            "invalid {case} must not mutate the original request"
        );
        finish(store);
    }
}
