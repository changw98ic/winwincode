// SPDX-License-Identifier: Apache-2.0

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use futures::future::BoxFuture;
use winwincode_execution_port::jev_decision::{ContextRetention, JevDecisionError, JevPolicy};
use winwincode_provider::{
    JevContextRequest, JevDevice, JevDtype, JevExecutionOptions, JevHypothesis, JevProviderError,
    JevProviderErrorKind, JevRemoteTransport, JevRuntime, JevRuntimeConfig, JevScores,
    OpenJevRemoteConfig, OpenJevRemoteConfigRequest, OpenJevRemoteProvider, RemoteJevScoreBatch,
};

#[derive(Debug)]
struct ContextTransport {
    calls: Arc<AtomicUsize>,
    available: bool,
    pending: bool,
    resolved_model: &'static str,
}

impl JevRemoteTransport for ContextTransport {
    fn score(
        &self,
        _model: String,
        inputs: Vec<JevHypothesis>,
        _options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<RemoteJevScoreBatch, JevProviderError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let available = self.available;
        let pending = self.pending;
        let resolved_model = self.resolved_model;
        Box::pin(async move {
            if pending {
                std::future::pending::<()>().await;
            }
            if !available {
                return Err(JevProviderError::new(JevProviderErrorKind::Unavailable));
            }
            assert_eq!(inputs.len(), 4);
            assert!(inputs.iter().all(|input| {
                let premise: serde_json::Value = serde_json::from_str(&input.premise).unwrap();
                premise["task"] == "repair parser" && premise["candidate"] == "obsolete log"
            }));
            Ok(RemoteJevScoreBatch {
                evaluations: inputs
                    .iter()
                    .map(|input| {
                        let entailment = if input.hypothesis.contains("safe to discard") {
                            0.95
                        } else {
                            0.05
                        };
                        JevScores::try_new(entailment, 0.0, 1.0 - entailment).unwrap()
                    })
                    .collect(),
                input_tokens: 31,
                output_tokens: Some(7),
                resolved_model_id: Some(resolved_model.to_owned()),
                device: JevDevice::Cpu,
            })
        })
    }
}

fn setup(
    available: bool,
    resolved_model: &'static str,
    pending: bool,
) -> (JevRuntime, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let config = OpenJevRemoteConfig::try_new(OpenJevRemoteConfigRequest {
        provider_id: "context-provider".to_owned(),
        endpoint: "https://nli.invalid/score".to_owned(),
        model_id: "configured-jev".to_owned(),
        max_batch_size: 4,
        devices: vec![JevDevice::Cpu],
        dtypes: vec![JevDtype::Float32],
        timeout: Duration::from_secs(1),
        api_key: None,
    })
    .unwrap();
    let provider = OpenJevRemoteProvider::new(
        config,
        Arc::new(ContextTransport {
            calls: Arc::clone(&calls),
            available,
            pending,
            resolved_model,
        }),
    );
    (
        JevRuntime::new(
            vec![Arc::new(provider)],
            JevRuntimeConfig {
                timeout: Duration::from_secs(1),
                retries: 0,
            },
        ),
        calls,
    )
}

fn policy() -> JevPolicy {
    JevPolicy {
        version: "test-policy".to_owned(),
        minimum_confidence: 0.6,
        pin_threshold: 0.8,
        keep_threshold: 0.8,
        compact_threshold: 0.8,
        drop_threshold: 0.8,
        task_memory_threshold: 0.5,
        project_memory_threshold: 0.7,
        long_term_memory_threshold: 0.9,
    }
}

fn request(protected: bool) -> JevContextRequest {
    JevContextRequest {
        task: "repair parser".to_owned(),
        candidate: "obsolete log".to_owned(),
        protected,
        archive_eligible: true,
    }
}

fn options() -> JevExecutionOptions {
    JevExecutionOptions {
        device: JevDevice::Cpu,
        dtype: JevDtype::Float32,
    }
}

#[test]
fn provider_scores_drive_retention_while_host_protection_and_accounting_survive() {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            let (runtime, calls) = setup(true, "resolved-jev", false);
            for (protected, expected) in [
                (false, ContextRetention::Drop),
                (true, ContextRetention::Pin),
            ] {
                let run = runtime
                    .evaluate_context(request(protected), &policy(), options())
                    .await
                    .unwrap();
                let evaluation = run.value.unwrap();
                assert_eq!(evaluation.decision.decision, expected);
                assert_eq!(evaluation.decision.model, "resolved-jev");
                assert!(evaluation.hypotheses.disposable.entailment > 0.9);
                let observation = run.observation.unwrap();
                assert_eq!(observation.batch_size, 4);
                assert_eq!(
                    (observation.input_tokens, observation.output_tokens),
                    (31, Some(7))
                );
                assert!(run.failures.is_empty());
            }
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert!(!format!("{:?}", request(false)).contains("obsolete log"));
        });
}

#[test]
fn invalid_policy_does_not_call_provider_and_outage_never_becomes_drop() {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            let (runtime, calls) = setup(false, "resolved-jev", false);
            let mut invalid = policy();
            invalid.drop_threshold = f64::NAN;
            assert_eq!(
                runtime
                    .evaluate_context(request(false), &invalid, options())
                    .await,
                Err(JevDecisionError::InvalidPolicy)
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let run = runtime
                .evaluate_context(request(false), &policy(), options())
                .await
                .unwrap();
            assert!(run.value.is_none());
            assert_eq!(run.failures.len(), 1);
            assert_eq!(run.failures[0].kind, JevProviderErrorKind::Unavailable);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
}

#[test]
fn invalid_decision_metadata_keeps_paid_usage_without_a_retention_action() {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            let (runtime, calls) = setup(true, "invalid\nmodel", false);
            let run = runtime
                .evaluate_context(request(false), &policy(), options())
                .await
                .unwrap();
            assert!(run.value.is_none());
            assert_eq!(run.failures[0].kind, JevProviderErrorKind::InvalidResponse);
            assert_eq!(run.observation.unwrap().input_tokens, 31);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
}

#[test]
fn device_context_receipt_replays_success_and_failure_after_reopen() {
    use winwincode_provider::{DeviceProviderStore, StoredJevContext};
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            for available in [true, false] {
                let directory = std::env::temp_dir()
                    .join(format!("wwc-jev-replay-{}-{available}", std::process::id()));
                let _ = std::fs::remove_dir_all(&directory);
                let configuration = format!("sha256:{}", "a".repeat(64));
                let (runtime, calls) = setup(available, "resolved-jev", false);
                let store = DeviceProviderStore::open(&directory).unwrap();
                let first = store
                    .evaluate_context_once(
                        "exchange/candidate",
                        &configuration,
                        &runtime,
                        request(false),
                        &policy(),
                        options(),
                    )
                    .await
                    .unwrap();
                let StoredJevContext::Completed {
                    run,
                    replayed: false,
                } = first
                else {
                    panic!("expected persisted inference");
                };
                assert_eq!(run.value.is_some(), available);
                assert_eq!(run.failures.is_empty(), available);
                drop(store);
                let store = DeviceProviderStore::open(&directory).unwrap();
                assert_eq!(
                    store
                        .evaluate_context_once(
                            "exchange/candidate",
                            &configuration,
                            &runtime,
                            request(false),
                            &policy(),
                            options()
                        )
                        .await
                        .unwrap(),
                    StoredJevContext::Completed {
                        run,
                        replayed: true
                    }
                );
                assert!(
                    store
                        .evaluate_context_once(
                            "exchange/candidate",
                            &configuration,
                            &runtime,
                            request(true),
                            &policy(),
                            options()
                        )
                        .await
                        .is_err()
                );
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                let db = rusqlite::Connection::open(directory.join("providers.sqlite3")).unwrap();
                let stored: String = db
                    .query_row("SELECT result FROM jev_context_exchanges", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert!(!stored.contains("obsolete log"));
                assert!(!stored.contains("repair parser"));
                drop(db);
                drop(store);
                std::fs::remove_dir_all(directory).unwrap();
            }
        });
}

#[test]
fn interrupted_device_context_inference_is_not_reissued_after_reopen() {
    use winwincode_provider::{DeviceProviderStore, StoredJevContext};
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            let directory =
                std::env::temp_dir().join(format!("wwc-jev-interrupted-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&directory);
            let configuration = format!("sha256:{}", "b".repeat(64));
            let (runtime, calls) = setup(true, "resolved-jev", true);
            let store = DeviceProviderStore::open(&directory).unwrap();
            let policy = policy();
            {
                let mut future = Box::pin(store.evaluate_context_once(
                    "interrupted",
                    &configuration,
                    &runtime,
                    request(false),
                    &policy,
                    options(),
                ));
                assert!(futures::poll!(&mut future).is_pending());
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
            drop(store);
            let store = DeviceProviderStore::open(&directory).unwrap();
            assert_eq!(
                store
                    .evaluate_context_once(
                        "interrupted",
                        &configuration,
                        &runtime,
                        request(false),
                        &policy,
                        options()
                    )
                    .await
                    .unwrap(),
                StoredJevContext::Incomplete
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            drop(store);
            std::fs::remove_dir_all(directory).unwrap();
        });
}

fn sealed_context_session(
    provider: &str,
) -> winwincode_execution_port::agent_config::AgentSessionConfigSnapshot {
    use winwincode_execution_port::agent_config::{
        AgentJevContextSettings, AgentProfileSettings, resolve_agent_session_config,
    };
    resolve_agent_session_config(
        &winwincode_domain::WorkerId("wrk_jev_context_fixture".into()),
        &serde_json::from_value(serde_json::json!({
            "capabilityDigest": format!("sha256:{}", "a".repeat(64)),
            "features": [], "maxConcurrentJobs": 1, "platform": "aarch64-apple-darwin"
        }))
        .unwrap(),
        "executor",
        AgentProfileSettings {
            fusion: None,
            jev_judge: None,
            jev_context: Some(AgentJevContextSettings {
                provider: provider.into(),
                policy: policy(),
            }),
            provider: "glm".into(),
            model: "glm-5.3-flash".into(),
            reasoning: "max".into(),
            tools: vec![],
            sandbox: "candidate".into(),
            instructions: None,
        },
    )
    .unwrap()
}

#[test]
fn configured_context_rejects_unsealed_or_foreign_provider_before_claim() {
    use winwincode_provider::{DeviceProviderStore, OpenJevRemoteSettings};
    let directory = std::env::temp_dir().join(format!("wwc-jev-profile-{}", std::process::id()));
    let store = DeviceProviderStore::open(&directory).unwrap();
    let settings = OpenJevRemoteSettings {
        provider_id: "device-jev".into(),
        endpoint: "https://nli.invalid/score".into(),
        api_key: Some("private-fixture-key".into()),
        model_id: Some("jev-fixture".into()),
        timeout_ms: 1000,
        retries: 0,
        max_batch_size: Some(4),
        devices: Some(vec!["remote".into()]),
        dtypes: Some(vec!["auto".into()]),
    };
    store.save_jev_settings(&settings).unwrap();
    let mut altered = sealed_context_session("device-jev");
    altered
        .profile
        .source
        .settings
        .jev_context
        .as_mut()
        .unwrap()
        .policy
        .drop_threshold = 0.95;
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            for session in [sealed_context_session("another-provider"), altered] {
                assert!(
                    store
                        .evaluate_configured_context_once(
                            "must-not-claim",
                            &session,
                            request(false)
                        )
                        .await
                        .is_err()
                );
            }
        });
    let database = rusqlite::Connection::open(directory.join("providers.sqlite3")).unwrap();
    let count: i64 = database
        .query_row("SELECT COUNT(*) FROM jev_context_exchanges", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    drop(database);
    drop(store);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn device_jev_settings_survive_restart_and_stay_out_of_public_state() {
    use winwincode_provider::{DeviceProviderStore, OpenJevRemoteSettings};
    let directory = std::env::temp_dir().join(format!("wwc-jev-settings-{}", std::process::id()));
    let store = DeviceProviderStore::open(&directory).unwrap();
    let mut settings = OpenJevRemoteSettings {
        provider_id: "device-jev".into(),
        endpoint: "https://nli.invalid/score".into(),
        api_key: Some("private-device-jev-key".into()),
        model_id: Some("jev-fixture".into()),
        timeout_ms: 1000,
        retries: 0,
        max_batch_size: Some(4),
        devices: Some(vec!["remote".into()]),
        dtypes: Some(vec!["auto".into()]),
    };
    let before = store.snapshot("device-1").unwrap();
    store.save_jev_settings(&settings).unwrap();
    drop(store);
    let store = DeviceProviderStore::open(&directory).unwrap();
    let saved = store.resolve_jev_settings("device-jev").unwrap();
    assert_eq!(
        serde_json::to_value(&saved).unwrap(),
        serde_json::to_value(&settings).unwrap()
    );
    assert_eq!(store.snapshot("device-1").unwrap(), before);
    assert!(!format!("{saved:?}").contains("private-device-jev-key"));
    assert!(store.resolve_jev_settings("missing").is_err());
    settings.endpoint = "http://nli.invalid/score".into();
    assert!(store.save_jev_settings(&settings).is_err());
    assert_eq!(
        store.resolve_jev_settings("device-jev").unwrap().endpoint,
        saved.endpoint
    );
    settings.endpoint = saved.endpoint;
    settings.api_key = Some("rotated-device-jev-key".into());
    store.save_jev_settings(&settings).unwrap();
    assert_eq!(
        store.resolve_jev_settings("device-jev").unwrap().api_key,
        settings.api_key
    );
    let database = rusqlite::Connection::open(directory.join("providers.sqlite3")).unwrap();
    database
        .execute("UPDATE jev_settings SET provider_id='foreign'", [])
        .unwrap();
    assert!(store.resolve_jev_settings("foreign").is_err());
    let claims: i64 = database
        .query_row("SELECT count(*) FROM jev_context_exchanges", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(claims, 0);
    drop(database);
    drop(store);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn persisted_remote_probabilities_roundtrip_exactly() {
    for probability in [0.920_000_016_689_300_5_f64, 0.959_999_978_542_327_9_f64] {
        let receipt = serde_json::to_string(&probability).unwrap();
        let replay: f64 = serde_json::from_str(&receipt).unwrap();
        assert_eq!(probability.to_bits(), replay.to_bits(), "{receipt}");
    }
}

#[test]
#[ignore = "requires Device-local SystemOne credentials and explicit live-run directory"]
fn configured_system_one_context_live_receipt_replays_after_restart() {
    use winwincode_provider::{DeviceProviderStore, OpenJevRemoteSettings, StoredJevContext};
    let required = |key| std::env::var(key).expect("live JEV setting is required");
    let directory = std::path::PathBuf::from(required("WWC_JEV_LIVE_DIRECTORY"));
    let provider = required("JEV_PROVIDER_ID");
    let settings = OpenJevRemoteSettings {
        provider_id: provider.clone(),
        endpoint: required("JEV_ENDPOINT"),
        api_key: Some(required("JEV_API_KEY")),
        model_id: Some(required("JEV_MODEL")),
        timeout_ms: required("JEV_TIMEOUT_MS").parse().unwrap(),
        retries: required("JEV_RETRIES").parse().unwrap(),
        max_batch_size: Some(4),
        devices: Some(vec!["remote".into()]),
        dtypes: Some(vec!["auto".into()]),
    };
    let session = sealed_context_session(&provider);
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            let store = DeviceProviderStore::open(&directory).unwrap();
            store.save_jev_settings(&settings).unwrap();
            let first = store
                .evaluate_configured_context_once("live-context-1", &session, request(true))
                .await
                .unwrap();
            let StoredJevContext::Completed { run, .. } = &first else {
                panic!("live inference was interrupted")
            };
            assert!(
                run.failures.is_empty(),
                "live provider returned a failure; retain the receipt"
            );
            let evaluation = run.value.as_ref().expect("live context decision");
            assert_eq!(evaluation.decision.decision, ContextRetention::Pin);
            assert_eq!(evaluation.decision.provider, provider);
            assert!(run.observation.is_some());
            drop(store);
            let store = DeviceProviderStore::open(&directory).unwrap();
            let replay = store
                .evaluate_configured_context_once("live-context-1", &session, request(true))
                .await
                .unwrap();
            let StoredJevContext::Completed {
                run: replayed,
                replayed: true,
            } = replay
            else {
                panic!("restart did not replay")
            };
            assert_eq!(run.as_ref(), replayed.as_ref());
        });
}
