// SPDX-License-Identifier: Apache-2.0

use super::tests::{delegated_record_and_binding, diagnostic_adapter_config};
use super::*;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use winwincode_fusion::{
    FusionBudget, FusionInput, FusionProviderCandidate, FusionProviderError,
    MapFusionProviderRouter, claims::default_claim_output_schema, run_blind_panel,
    validate_panel_result,
};
use winwincode_kernel::{ModelPort, ModelPortFailure, ModelPortRequest, ModelPortStream};

#[derive(Debug, Default)]
struct MemberPort(Mutex<Vec<Value>>);

impl ModelPort for MemberPort {
    fn stream(
        &self,
        request: ModelPortRequest,
    ) -> BoxFuture<'static, Result<ModelPortStream, ModelPortFailure>> {
        let payload: Value = serde_json::from_str(&request.payload_json).unwrap();
        let bad_member = payload["request"]["model"] == "bad-member";
        self.0.lock().unwrap().push(payload);
        Box::pin(async move {
            let reference = if bad_member {
                "file:first\nfile:second"
            } else {
                "file:fixture.rs"
            };
            let answer = json!({"claims":[{"claimKey":"claim:fixture", "summary":"Fixture claim",
                "position":"supports","evidence":[{"evidenceType":"file","sourceRef":reference}],
                "requiredEvidence":["test"]}]});
            let frames = [
                json!({"type":"output_item_done","item":{"type":"message","role":"assistant",
                    "content":[{"type":"output_text","text":answer.to_string()}]}})
                .to_string(),
                json!({"type":"completed","responseId":request.request_id,
                    "tokenUsage":{"input_tokens":7,"output_tokens":3}})
                .to_string(),
            ];
            Ok(Box::pin(futures::stream::iter(frames.into_iter().map(Ok))) as ModelPortStream)
        })
    }
}

#[tokio::test]
async fn shared_claim_admission_isolates_invalid_member_before_real_aggregation() {
    let port = Arc::new(MemberPort::default());
    let provider = Arc::new(crate::FusionModelPortProvider::new(
        port.clone(),
        "fixture-session".into(),
        "fixture-thread".into(),
        "fixture-turn".into(),
    ));
    let mut router = MapFusionProviderRouter::new();
    let mut candidates = Vec::new();
    for member in ["left", "middle", "right", "bad"] {
        let route = format!("route-{member}");
        router = router.with(&route, provider.clone());
        candidates.push(FusionProviderCandidate {
            id: member.into(),
            provider: route,
            model: if member == "bad" {
                "bad-member"
            } else {
                "valid-member"
            }
            .into(),
            reasoning_effort: Some("max".into()),
        });
    }
    let input = FusionInput {
        question: "Review fixture candidate".into(),
        canonical_context: json!({"fixture":true}),
        constraints: Vec::new(),
        expected_output_schema: default_claim_output_schema(),
        provider_candidates: candidates,
        budget: FusionBudget::default(),
    };
    let panel = run_blind_panel("fixture-panel", input.clone(), Arc::new(router))
        .await
        .unwrap();
    validate_panel_result("fixture-panel", &input, &panel).unwrap();
    let aggregate = crate::durable_fusion::aggregation_prompt("Review fixture", &panel);
    assert_eq!(port.0.lock().unwrap().len(), 4);
    let requests = port.0.lock().unwrap();
    let ids: BTreeSet<_> = requests
        .iter()
        .map(|request| request["requestId"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 4);
    assert!(
        requests
            .iter()
            .all(|request| request["threadId"] == "fixture-thread")
    );
    assert_eq!(panel.candidates.len(), 3);
    assert_eq!(panel.failures.len(), 1);
    assert_eq!(panel.failures[0].candidate_id, "bad");
    assert_eq!(panel.failures[0].code, "FUSION_INVALID_CLAIMS");
    assert!(
        aggregate.is_ok(),
        "valid siblings must reach real aggregation"
    );
}

async fn retained_poll_failure(code: &str) -> (String, Value) {
    let root = std::env::temp_dir().join(format!("wwc-fusion-code-{}", Uuid::new_v4()));
    let (mut record, mut binding) = delegated_record_and_binding();
    binding.authority.lease.fencing_token = winwincode_domain::FencingToken("1".into());
    binding.authority.lease.lease_id =
        winwincode_domain::LeaseId("lse_00000000000000000000000001".into());
    binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
    binding.authority.lease.expires_at = Instant("2026-08-28T01:00:00.000Z".into());
    binding.opened_at = binding.authority.lease.issued_at.clone();
    record.last_activity_at = binding.opened_at.clone();
    record.workspace = root.join("candidate");
    std::fs::create_dir_all(&record.workspace).unwrap();
    record.phase = StoredRunPhase::Prepared;
    record.current_turn_id = None;
    let config = diagnostic_adapter_config(&root);
    record.agent_config = production_agent_session_config(
        &config,
        &binding.authority.lease.worker_id,
        &record.job,
        record.role_policy.as_ref(),
    )
    .unwrap();
    let key = binding.run_key.clone();
    let thread = record.canonical_thread_id.clone();
    let mut adapter = ProductionCodexAdapter::open(config).unwrap();
    adapter
        .install_active_run(&key, record, binding, false, false)
        .unwrap();
    adapter
        .observe_now(&Instant("2026-08-28T00:00:00.000Z".into()))
        .unwrap();
    let code = code.to_owned();
    adapter.runs.get_mut(&key).unwrap().pending_fusion = Some(Box::pin(async move {
        Err(FusionProviderError::new(
            code,
            "synthetic private response detail",
        ))
    }));
    assert!(adapter.poll_fusion_panel(&thread).await.unwrap());
    let stored = load_stored_run(&adapter.store, &key).unwrap().unwrap();
    assert!(matches!(
        stored.terminal,
        Some(StoredTerminal::InfrastructureFailed { .. })
    ));
    let cause = stored.infrastructure_failure_code.unwrap();
    let diagnostic = serde_json::to_value(stored.failure_diagnostic.unwrap()).unwrap();
    drop(adapter);
    std::fs::remove_dir_all(root).unwrap();
    (cause, diagnostic)
}

#[tokio::test]
async fn shared_claim_admission_poll_preserves_known_safe_failure_code() {
    let (cause, diagnostic) = retained_poll_failure("FUSION_INVALID_CLAIMS").await;
    assert_eq!(cause, "FUSION_INVALID_CLAIMS");
    assert_eq!(diagnostic["cause"], "FUSION_INVALID_CLAIMS");
    assert_eq!(diagnostic["stage"], "adapter_operation");
}

#[tokio::test]
async fn shared_claim_admission_poll_rejects_unknown_or_private_failure_codes() {
    for code in [
        "UNKNOWN_CODE",
        "FUSION_INVALID_CLAIMS private-detail",
        "secret\nvalue",
    ] {
        let (cause, diagnostic) = retained_poll_failure(code).await;
        assert_eq!(cause, "FUSION_PANEL_FAILED");
        assert_eq!(diagnostic["cause"], "FUSION_PANEL_FAILED");
        let safe = diagnostic.to_string();
        assert!(!safe.contains("private"));
        assert!(!safe.contains("secret"));
    }
}
