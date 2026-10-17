// SPDX-License-Identifier: Apache-2.0

//! Ordinary Chat has its own sealed task scope; Delivery keeps its exact plan gate.

use super::*;
use crate::device_execution_source::{
    ProviderConnectionMaterial, ProviderExecutionPurpose, ProviderExecutionRequest,
    ProviderExecutionSource, ProviderJevParameters, ProviderModelParameters,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
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

fn store_and_request(job: &ExecutionJob, label: &str) -> (ProviderRuntimeStore, serde_json::Value) {
    let root =
        std::env::temp_dir().join(format!("wwc-jev-chat-scope-{label}-{}", std::process::id()));
    let config = DeviceProviderStore::open(&root).unwrap();
    let settings: crate::OpenJevRemoteSettings = serde_json::from_value(serde_json::json!({
        "providerId":"fixture-jev-chat", "endpoint":"https://nli.invalid/score", "apiKey":"offline-only",
        "modelId":"fixture-jev-model", "timeoutMs":1000,"retries":0,"devices":["remote"],"dtypes":["auto"]
    })).unwrap();
    config.save_jev_settings(&settings).unwrap();
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
    (ProviderRuntimeStore::prepare(&root).unwrap(), request)
}

fn finish(store: ProviderRuntimeStore) {
    let root = store.directory().parent().unwrap().to_path_buf();
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

#[derive(Debug, Default)]
struct UnavailableJevSource {
    jev_parameters: AtomicUsize,
    model_connections: AtomicUsize,
}

impl ProviderExecutionSource for UnavailableJevSource {
    fn model_parameters(
        &self,
        request: &ProviderExecutionRequest,
    ) -> Result<ProviderModelParameters, DeviceProviderError> {
        Ok(ProviderModelParameters {
            provider_id: request.provider_id.clone(),
            endpoint: "https://fixture.invalid/responses".into(),
            protocol: winwincode_api::generated::DeviceProviderProtocol::Canonical,
            responses_structured_output: None,
        })
    }

    fn jev_parameters(
        &self,
        _: &ProviderExecutionRequest,
    ) -> Result<ProviderJevParameters, DeviceProviderError> {
        self.jev_parameters.fetch_add(1, Ordering::SeqCst);
        Err(DeviceProviderError::StateInvalid)
    }

    fn resolve_connection(
        &self,
        request: &ProviderExecutionRequest,
        _: &dyn Fn() -> bool,
    ) -> Result<ProviderConnectionMaterial, DeviceProviderError> {
        assert_eq!(request.purpose, ProviderExecutionPurpose::Model);
        self.model_connections.fetch_add(1, Ordering::SeqCst);
        Err(DeviceProviderError::StateInvalid)
    }
}

fn unavailable_jev_request(
    label: &str,
    judge: bool,
) -> (
    ProviderRuntimeStore,
    ModelOpenMessage,
    serde_json::Value,
    Arc<UnavailableJevSource>,
) {
    let (open, job) = fixture();
    let (mut store, mut request) = store_and_request(&job, label);
    let source = Arc::new(UnavailableJevSource::default());
    store.execution_source = Some(source.clone());
    if judge {
        let session: winwincode_execution_port::agent_config::AgentSessionConfigSnapshot =
            serde_json::from_value(request["winwincodeJevContext"].clone()).unwrap();
        let mut settings = session.profile.source.settings.clone();
        settings.jev_judge = Some("fixture-jev-chat".into());
        request["winwincodeJevContext"] = serde_json::to_value(
            resolve_agent_session_config(
                &session.identity.worker_id,
                &session.identity.capabilities,
                &session.profile.source.execution_profile,
                settings,
            )
            .unwrap(),
        )
        .unwrap();
        request["winwincodeJevJudge"] = serde_json::json!([]);
    }
    (store, open, request, source)
}

fn assert_zero_jev_work_reaches_model(label: &str, commentary: bool, judge: bool) {
    use base64::Engine as _;

    let (store, mut open, mut request, source) = unavailable_jev_request(label, judge);
    if !commentary {
        request["request"]["input"].as_array_mut().unwrap().pop();
    }
    let original_input = request["request"]["input"].clone();
    let payload = serde_json::to_vec(&request).unwrap();
    open.request.content_type = "application/json".into();
    open.request.data_base64 = base64::engine::general_purpose::STANDARD.encode(&payload);
    open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(&payload));
    let chunks = store.execute_model(&open).unwrap();
    assert_eq!(source.jev_parameters.load(Ordering::SeqCst), 0, "{label}");
    assert_eq!(
        source.model_connections.load(Ordering::SeqCst),
        1,
        "{label}"
    );
    assert_eq!(
        chunks.last().unwrap().error.as_ref().unwrap().message,
        "DEVICE_PROVIDER_STATE_INVALID",
        "{label} reaches the main model connection fixture"
    );
    let prepared: Vec<u8> = store
        .connection
        .query_row(
            "SELECT prepared_payload FROM exchanges WHERE exchange_id=?1",
            [&open.model_exchange_id.0],
            |row| row.get(0),
        )
        .unwrap();
    let prepared: serde_json::Value = serde_json::from_slice(&prepared).unwrap();
    assert_eq!(prepared["request"]["input"], original_input);
    for field in [
        "winwincodeJevContext",
        "winwincodeJevTask",
        "winwincodeJevJudge",
    ] {
        assert!(prepared.get(field).is_none(), "{label}: {field}");
    }
    assert_eq!(store.execute_model(&open).unwrap(), chunks);
    assert_eq!(source.jev_parameters.load(Ordering::SeqCst), 0);
    assert_eq!(source.model_connections.load(Ordering::SeqCst), 1);
    finish(store);
}

#[test]
fn jev_preparation_zero_commentary_reaches_model_without_jev_parameters() {
    assert_zero_jev_work_reaches_model("zero-commentary", false, false);
}

#[test]
fn jev_preparation_retained_last_commentary_reaches_model_without_jev_parameters() {
    assert_zero_jev_work_reaches_model("retained-commentary", true, false);
}

#[test]
fn jev_preparation_empty_judge_reaches_model_without_jev_parameters() {
    assert_zero_jev_work_reaches_model("empty-judge", true, true);
}

#[test]
fn jev_preparation_invalid_input_is_rejected_before_jev_parameters() {
    for case in ["context-input", "judge-input", "missing-judge"] {
        let (store, open, mut request, source) = unavailable_jev_request(case, true);
        match case {
            "context-input" => request["request"]["input"] = serde_json::json!({}),
            "judge-input" => request["winwincodeJevJudge"] = serde_json::json!({}),
            "missing-judge" => {
                request
                    .as_object_mut()
                    .unwrap()
                    .remove("winwincodeJevJudge");
            }
            _ => unreachable!(),
        }
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
        assert_eq!(source.jev_parameters.load(Ordering::SeqCst), 0, "{case}");
        assert_eq!(source.model_connections.load(Ordering::SeqCst), 0, "{case}");
        finish(store);
    }
}

#[test]
fn jev_preparation_real_work_requires_available_jev_parameters() {
    use base64::Engine as _;

    for judge in [false, true] {
        let label = if judge { "judge-work" } else { "context-work" };
        let (store, mut open, mut request, source) = unavailable_jev_request(label, judge);
        if judge {
            request["winwincodeJevJudge"] = serde_json::json!([
                {"claimKey":"claim","premise":"The tool failed","hypothesis":"The tool passed"}
            ]);
        } else {
            let candidate = request["request"]["input"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .clone();
            request["request"]["input"]
                .as_array_mut()
                .unwrap()
                .insert(1, candidate);
        }
        let payload = serde_json::to_vec(&request).unwrap();
        open.request.content_type = "application/json".into();
        open.request.data_base64 = base64::engine::general_purpose::STANDARD.encode(&payload);
        open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(&payload));
        let chunks = store.execute_model(&open).unwrap();
        assert_eq!(source.jev_parameters.load(Ordering::SeqCst), 1, "{label}");
        assert_eq!(
            source.model_connections.load(Ordering::SeqCst),
            0,
            "{label}"
        );
        assert_eq!(
            chunks.last().unwrap().error.as_ref().unwrap().message,
            "DEVICE_JEV_UNAVAILABLE",
            "{label}"
        );
        finish(store);
    }
}
