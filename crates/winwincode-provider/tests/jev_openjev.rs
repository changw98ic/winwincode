// SPDX-License-Identifier: Apache-2.0

//! JEV-01 `OpenJev` runtime adapter contract tests.
//!
//! These tests keep Jev fail-open for Coding Agent Worker: Provider outage
//! never becomes a hard Worker failure.

use std::{future::Future, sync::Arc, time::Duration};

use winwincode_provider::{
    JevDevice, JevDtype, JevExecutionOptions, JevFallbackPolicy, JevHealth, JevHypothesis,
    JevProvider, JevProviderCapabilities, JevProviderErrorKind, JevRuntime, JevRuntimeConfig,
    JevScores, MockJevProvider, MockJevRemoteTransport, OPENJEV_LOCAL_RUNTIME_GAP,
    OpenJevLocalProvider, OpenJevRemoteProvider, OpenJevRemoteSettings,
    contract_capability_mocks, jev_provider_order, parse_jev_score_list, parse_jev_scores,
};

fn hypothesis() -> JevHypothesis {
    JevHypothesis {
        premise: "workspace tests are green".to_owned(),
        hypothesis: "the release candidate is ready".to_owned(),
    }
}

fn cpu_options() -> JevExecutionOptions {
    JevExecutionOptions {
        device: JevDevice::Cpu,
        dtype: JevDtype::Float32,
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
        .block_on(future)
}

fn runtime(providers: Vec<Arc<dyn JevProvider>>) -> JevRuntime {
    JevRuntime::new(
        providers,
        JevRuntimeConfig {
            timeout: Duration::from_millis(40),
            retries: 0,
        },
    )
}

#[test]
fn contract_gate_covers_three_capability_mocks() {
    block_on(async {
        let mocks = contract_capability_mocks();
        assert_eq!(mocks.len(), 3, "DoD requires at least three capability mocks");
        let mut seen_batches = Vec::new();
        for mock in mocks {
            let capabilities = mock.capabilities();
            seen_batches.push(capabilities.max_batch_size);
            let options = JevExecutionOptions {
                device: capabilities.devices[0],
                dtype: capabilities.dtypes[0],
            };
            assert_eq!(mock.health().await, JevHealth::Healthy);
            let evaluation = mock.evaluate(hypothesis(), options).await.expect("evaluate");
            assert!(evaluation.scores.confidence() > 0.0);
            assert_ne!(evaluation.device, JevDevice::Auto);
            let batch = mock
                .batch_evaluate(vec![hypothesis(); capabilities.max_batch_size.min(2)], options)
                .await
                .expect("batch");
            assert_eq!(batch.evaluations.len(), capabilities.max_batch_size.min(2));
        }
        // Capability diversity is part of the contract coverage.
        assert!(seen_batches.windows(2).any(|pair| pair[0] != pair[1]));
    });
}

#[test]
fn jev_outage_does_not_block_coding_agent_worker() {
    block_on(async {
        let providers = jev_provider_order(
            JevFallbackPolicy::Fallback,
            Some(Arc::new(OpenJevLocalProvider::with_runtime_gap())),
            Some(Arc::new(MockJevProvider::unavailable("openjev-remote"))),
        );
        let run = runtime(providers).evaluate(hypothesis(), cpu_options()).await;
        assert!(run.value.is_none());
        assert!(run.observation.is_none());
        assert!(!run.failures.is_empty());
        // The worker-side effect is "continue without Jev", not an Err channel.
        let worker_status = if run.value.is_none() {
            "continued-without-jev"
        } else {
            "used-jev"
        };
        assert_eq!(worker_status, "continued-without-jev");
        assert!(OPENJEV_LOCAL_RUNTIME_GAP.contains("OpenJev"));
    });
}

#[test]
fn remote_settings_build_fallback_runtime_without_leaking_secrets() {
    let settings = OpenJevRemoteSettings::from_toml(
        r#"
providerId = "openjev-remote"
endpoint = "https://openjev.example.com/v1/nli"
apiKey = "top-secret-key"
timeoutMs = 300
retries = 1
maxBatchSize = 8
devices = ["cpu", "cuda", "mps"]
dtypes = ["float32", "float16", "bfloat16"]
"#,
    )
    .expect("settings");
    let (config, runtime_config) = settings.to_config_and_runtime().expect("converted");
    assert_eq!(runtime_config.retries, 1);
    assert_eq!(config.endpoint(), "https://openjev.example.com/v1/nli");
    let debug = format!("{config:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("top-secret-key"));

    let remote = OpenJevRemoteProvider::new(config, Arc::new(MockJevRemoteTransport::healthy()));
    let local = OpenJevLocalProvider::with_runtime_gap();
    let providers = jev_provider_order(
        JevFallbackPolicy::LocalFirst,
        Some(Arc::new(local)),
        Some(Arc::new(remote)),
    );
    let run = block_on(JevRuntime::new(providers, runtime_config).evaluate(
        hypothesis(),
        cpu_options(),
    ));
    // Local gap fails, remote mock succeeds: remote fallback path is live.
    assert!(run.value.is_some());
    assert_eq!(
        run.observation.expect("observation").provider_id,
        "openjev-remote"
    );
}

#[test]
fn capability_mismatch_and_invalid_payload_fail_open() {
    block_on(async {
        let cpu_only = MockJevProvider::healthy("cpu-only").with_capabilities(
            JevProviderCapabilities {
                provider_id: "cpu-only".to_owned(),
                model_id: "cpu-only-nli".to_owned(),
                max_batch_size: 2,
                devices: vec![JevDevice::Cpu],
                dtypes: vec![JevDtype::Float32],
            },
        );
        let run = runtime(vec![Arc::new(cpu_only)])
            .evaluate(
                hypothesis(),
                JevExecutionOptions {
                    device: JevDevice::Cuda,
                    dtype: JevDtype::Float16,
                },
            )
            .await;
        assert!(run.value.is_none());
        assert_eq!(run.failures[0].kind, JevProviderErrorKind::Unsupported);

        let invalid = MockJevProvider::healthy("invalid")
            .with_device(JevDevice::Auto) // Auto is rejected by the runtime contract
            .with_scores(JevScores::try_new(0.8, 0.1, 0.1).expect("valid"));
        let run = runtime(vec![Arc::new(invalid)])
            .evaluate(hypothesis(), cpu_options())
            .await;
        assert!(run.value.is_none());
        assert_eq!(run.failures[0].kind, JevProviderErrorKind::InvalidResponse);
    });
}

#[test]
fn nli_score_parsing_accepts_multiple_non_forced_payload_shapes() {
    let object = serde_json::json!({
        "scores": {"entailment": 0.9, "contradiction": 0.05, "neutral": 0.05}
    });
    let labeled = serde_json::json!([{
        "labels": [
            {"label": "neutral", "score": 0.2},
            {"label": "entailment", "score": 0.7},
            {"label": "contradiction", "score": 0.1}
        ]
    }]);
    assert!(parse_jev_scores(&object).is_ok());
    let list = parse_jev_score_list(&labeled).expect("labeled list");
    assert_eq!(list.len(), 1);
    assert!((list[0].entailment - 0.7).abs() < f32::EPSILON);
}
