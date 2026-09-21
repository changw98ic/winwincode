// SPDX-License-Identifier: Apache-2.0

//! FUSION-01 acceptance tests for the blind panel contract.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use futures::future::BoxFuture;
use serde_json::json;
use tokio::sync::Barrier;
use winwincode_fusion::FusionBudget;
use winwincode_fusion::FusionInput;
use winwincode_fusion::FusionProvider;
use winwincode_fusion::FusionProviderAnswer;
use winwincode_fusion::FusionProviderCandidate;
use winwincode_fusion::FusionProviderError;
use winwincode_fusion::FusionProviderRequest;
use winwincode_fusion::FusionProviderRouter;
use winwincode_fusion::FusionTokenUsage;
use winwincode_fusion::MapFusionProviderRouter;
use winwincode_fusion::run_blind_panel;

#[derive(Debug, Clone, Copy)]
enum Behavior {
    Answer(&'static str),
    Fail(&'static str),
    Hang,
    TokenOverrun,
}

#[derive(Debug)]
struct RecordingProvider {
    name: &'static str,
    behavior: Behavior,
    barrier: Arc<Barrier>,
    requests: Arc<Mutex<Vec<FusionProviderRequest>>>,
}

impl FusionProvider for RecordingProvider {
    fn complete(
        &self,
        request: FusionProviderRequest,
    ) -> BoxFuture<'static, Result<FusionProviderAnswer, FusionProviderError>> {
        self.requests.lock().expect("request log").push(request);
        let barrier = Arc::clone(&self.barrier);
        let name = self.name;
        let behavior = self.behavior;
        Box::pin(async move {
            barrier.wait().await;
            match behavior {
                Behavior::Answer(answer) => Ok(FusionProviderAnswer {
                    provider_response_id: format!("response-{name}"),
                    answer: json!({ "answer": answer }),
                    token_usage: Some(FusionTokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 15,
                    }),
                }),
                Behavior::Fail(code) => Err(FusionProviderError::new(
                    code,
                    format!("{name} is unavailable"),
                )),
                Behavior::Hang => futures::future::pending().await,
                Behavior::TokenOverrun => Ok(FusionProviderAnswer {
                    provider_response_id: format!("response-{name}"),
                    answer: json!({ "answer": "too-expensive" }),
                    token_usage: Some(FusionTokenUsage {
                        input_tokens: 900,
                        output_tokens: 200,
                        total_tokens: 1_100,
                    }),
                }),
            }
        })
    }
}

fn sample_input() -> FusionInput {
    FusionInput {
        question: "Which change fixes the root cause?".to_owned(),
        canonical_context: json!({ "repository": "fixture", "revision": "abc123" }),
        constraints: vec!["Cite only the supplied context.".to_owned()],
        expected_output_schema: json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"],
            "additionalProperties": false,
        }),
        provider_candidates: ["a", "b", "c"]
            .into_iter()
            .map(|suffix| FusionProviderCandidate {
                id: format!("candidate-{suffix}"),
                provider: format!("provider-{suffix}"),
                model: format!("model-{suffix}"),
                reasoning_effort: Some("high".to_owned()),
            })
            .collect(),
        budget: FusionBudget {
            candidate_timeout_millis: 200,
            max_total_tokens: 1_000,
        },
    }
}

fn router(
    barrier: &Arc<Barrier>,
    requests: &Arc<Mutex<Vec<FusionProviderRequest>>>,
    behaviors: [(&'static str, Behavior); 3],
) -> Arc<dyn FusionProviderRouter> {
    let mut map = MapFusionProviderRouter::new();
    for (name, behavior) in behaviors {
        map = map.with(
            name,
            Arc::new(RecordingProvider {
                name,
                behavior,
                barrier: Arc::clone(barrier),
                requests: Arc::clone(requests),
            }),
        );
    }
    Arc::new(map)
}

#[tokio::test]
async fn three_providers_run_in_parallel_with_isolated_blind_contexts() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(3));
    let panel_router = router(
        &barrier,
        &requests,
        [
            ("provider-a", Behavior::Answer("independent-a")),
            ("provider-b", Behavior::Answer("independent-b")),
            ("provider-c", Behavior::Answer("independent-c")),
        ],
    );

    let result = run_blind_panel("panel-1", sample_input(), panel_router)
        .await
        .expect("valid panel");

    assert_eq!(result.candidates.len(), 3);
    assert!(result.failures.is_empty());
    assert_eq!(
        result.candidates[0].answer,
        json!({ "answer": "independent-a" })
    );
    assert_eq!(
        result.candidates[1].answer,
        json!({ "answer": "independent-b" })
    );
    assert_eq!(
        result.candidates[2].answer,
        json!({ "answer": "independent-c" })
    );

    let captured = requests.lock().expect("request log");
    assert_eq!(captured.len(), 3, "all three Providers were contacted");

    let shared_prompts: HashSet<String> = captured
        .iter()
        .map(|request| serde_json::to_string(&request.prompt).expect("prompt serializes"))
        .collect();
    assert_eq!(shared_prompts.len(), 1);

    let request_ids: HashSet<&str> = captured
        .iter()
        .map(|request| request.request_id.as_str())
        .collect();
    assert_eq!(request_ids.len(), 3);
    for request in captured.iter() {
        let prompt_value = serde_json::to_value(&request.prompt).expect("prompt value");
        let keys: Vec<&String> = prompt_value
            .as_object()
            .expect("object prompt")
            .keys()
            .collect();
        assert_eq!(
            keys,
            [
                "canonicalContext",
                "constraints",
                "expectedOutputSchema",
                "question",
            ]
        );
        assert_eq!(request.max_total_tokens, 1_000);
        assert_eq!(request.panel_id, "panel-1");
        assert_ne!(request.candidate_id, "");
    }

    let mut request_ids: Vec<&str> = result
        .candidates
        .iter()
        .map(|candidate| candidate.audit.request_id.as_str())
        .collect();
    request_ids.sort_unstable();
    request_ids.dedup();
    assert_eq!(request_ids.len(), 3);
    for candidate in &result.candidates {
        assert_eq!(candidate.audit.input_digest, result.input_digest);
        assert!(candidate.audit.input_digest.starts_with("sha256:"));
        assert!(
            candidate
                .audit
                .request_payload_digest
                .starts_with("sha256:")
        );
        assert!(
            candidate
                .audit
                .provider_response_id
                .starts_with("response-")
        );
        assert!(candidate.audit.token_usage.is_some());
    }
}

#[tokio::test]
async fn timeout_and_provider_failure_do_not_kill_successful_siblings() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(3));
    let panel_router = router(
        &barrier,
        &requests,
        [
            ("provider-a", Behavior::Answer("survives")),
            ("provider-b", Behavior::Fail("PROVIDER_UNAVAILABLE")),
            ("provider-c", Behavior::Hang),
        ],
    );

    let mut input = sample_input();
    input.budget.candidate_timeout_millis = 80;
    let result = run_blind_panel("panel-2", input, panel_router)
        .await
        .expect("panel survives partial failure");

    assert_eq!(result.candidates.len(), 1);
    assert_eq!(result.failures.len(), 2);
    assert_eq!(result.candidates[0].answer, json!({ "answer": "survives" }));
    assert_eq!(result.candidates[0].audit.candidate_id, "candidate-a");
    assert_eq!(result.failures[0].candidate_id, "candidate-b");
    assert_eq!(result.failures[0].code, "PROVIDER_UNAVAILABLE");
    assert_eq!(result.failures[1].candidate_id, "candidate-c");
    assert_eq!(result.failures[1].code, "TIMEOUT");
}

#[tokio::test]
async fn token_overrun_is_an_isolated_failure() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(3));
    let panel_router = router(
        &barrier,
        &requests,
        [
            ("provider-a", Behavior::Answer("cheap")),
            ("provider-b", Behavior::TokenOverrun),
            ("provider-c", Behavior::Answer("also-cheap")),
        ],
    );

    let result = run_blind_panel("panel-3", sample_input(), panel_router)
        .await
        .expect("panel survives token overrun");

    assert_eq!(result.candidates.len(), 2);
    assert_eq!(result.failures.len(), 1);
    assert_eq!(result.failures[0].code, "TOKEN_BUDGET_EXCEEDED");
    assert_eq!(result.failures[0].candidate_id, "candidate-b");
}

#[tokio::test]
async fn missing_router_adapter_is_an_isolated_failure() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(2));
    let panel_router = router(
        &barrier,
        &requests,
        [
            ("provider-a", Behavior::Answer("from-a")),
            ("provider-b", Behavior::Answer("from-b")),
            ("provider-unused", Behavior::Answer("unused")),
        ],
    );

    let result = run_blind_panel("panel-4", sample_input(), panel_router)
        .await
        .expect("panel survives unresolved Provider");

    assert_eq!(result.candidates.len(), 2);
    assert_eq!(result.failures.len(), 1);
    assert_eq!(result.failures[0].candidate_id, "candidate-c");
    assert_eq!(result.failures[0].code, "PROVIDER_UNRESOLVED");
}

#[tokio::test]
async fn rejects_panels_without_three_distinct_providers() {
    let panel_router: Arc<dyn FusionProviderRouter> = Arc::new(MapFusionProviderRouter::new());
    let mut input = sample_input();
    input.provider_candidates[2].provider = "provider-b".to_owned();
    let error = run_blind_panel("panel-5", input, panel_router)
        .await
        .expect_err("duplicate Provider must be rejected");
    assert_eq!(
        error.to_string(),
        "Fusion panels require at least three distinct Providers"
    );

    let mut input = sample_input();
    input.provider_candidates.truncate(2);
    let panel_router: Arc<dyn FusionProviderRouter> = Arc::new(MapFusionProviderRouter::new());
    let error = run_blind_panel("panel-6", input, panel_router)
        .await
        .expect_err("fewer than three routes must be rejected");
    assert_eq!(
        error.to_string(),
        "Fusion panels require three to sixteen Providers"
    );
}

#[tokio::test]
async fn contract_wire_names_use_camel_case() {
    let input = sample_input();
    let value = serde_json::to_value(&input).expect("input serializes");
    let object = value.as_object().expect("object input");
    for key in [
        "question",
        "canonicalContext",
        "constraints",
        "expectedOutputSchema",
        "providerCandidates",
        "budget",
    ] {
        assert!(object.contains_key(key), "missing wire field {key}");
    }
    let candidate = &object["providerCandidates"][0];
    assert_eq!(candidate["reasoningEffort"], json!("high"));
    assert_eq!(object["budget"]["candidateTimeoutMillis"], json!(200));
    assert_eq!(object["budget"]["maxTotalTokens"], json!(1_000));

    let decoded: FusionInput = serde_json::from_value(value).expect("input deserializes");
    assert_eq!(decoded.question, input.question);
}
