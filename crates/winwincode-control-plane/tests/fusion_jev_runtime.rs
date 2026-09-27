// SPDX-License-Identifier: Apache-2.0

use futures::future::BoxFuture;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use winwincode_control_plane::{
    fusion_analysis::{FusionClaimPosition, FusionStopReason},
    fusion_compose::*,
};
use winwincode_fusion::evidence::{
    ClaimVerification, SourceReceipt, SourceReceiptKind, VerificationConclusion,
};
use winwincode_fusion::*;
use winwincode_provider::*;

#[derive(Debug)]
struct Answer(&'static str);
impl FusionProvider for Answer {
    fn complete(
        &self,
        _: FusionProviderRequest,
    ) -> BoxFuture<'static, Result<FusionProviderAnswer, FusionProviderError>> {
        let position = self.0;
        Box::pin(async move {
            Ok(FusionProviderAnswer {
                provider_response_id: position.into(),
                answer: json!({"claims":[{"claimKey":"claim:behavior", "summary":"Behavior is correct", "position":position, "evidence":[]}]}),
                token_usage: Some(FusionTokenUsage {
                    input_tokens: 1_000_000,
                    output_tokens: 1_000_000,
                    total_tokens: 2_000_000,
                }),
            })
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    Timeout,
    Transport,
    Invalid,
}
#[derive(Debug)]
struct FaultTransport {
    fault: Fault,
    inputs: Arc<Mutex<Vec<JevHypothesis>>>,
}
impl JevRemoteTransport for FaultTransport {
    fn score(
        &self,
        _: String,
        items: Vec<JevHypothesis>,
        _: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<RemoteJevScoreBatch, JevProviderError>> {
        self.inputs.lock().unwrap().extend(items);
        let fault = self.fault;
        Box::pin(async move {
            match fault {
                Fault::Timeout => std::future::pending().await,
                Fault::Transport => Err(JevProviderError::new(JevProviderErrorKind::Unavailable)),
                Fault::Invalid => Ok(RemoteJevScoreBatch {
                    evaluations: vec![],
                    input_tokens: 0,
                    device: JevDevice::Cpu,
                }),
            }
        })
    }
}

#[derive(Debug, Default)]
struct Investigation(AtomicUsize);
impl FusionInvestigationPort for Investigation {
    fn investigate(
        &self,
        request: InvestigationRequest,
    ) -> BoxFuture<'static, Result<Vec<InvestigationAnswer>, FusionComposeError>> {
        let ordinal = self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            assert!(
                ordinal < 2,
                "receipt-backed evidence did not reach verification"
            );
            // First action has only prose. It must not end the run or call JEV.
            if ordinal == 0 {
                return Ok(vec![]);
            }
            let mut items = Vec::new();
            for (name, side, conclusion) in [
                (
                    "support",
                    FusionClaimPosition::Supports,
                    VerificationConclusion::Support,
                ),
                (
                    "counter",
                    FusionClaimPosition::Opposes,
                    VerificationConclusion::Counter,
                ),
            ] {
                let output = std::process::Command::new("/bin/sh")
                    .args(["-c", &format!("printf '{name}'")])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                let receipt_id = format!("command-{name}");
                items.push(InvestigationEvidenceItem {
                    kind: "command".into(),
                    detail: String::from_utf8(output.stdout.clone()).unwrap(),
                    side,
                    source_receipt: SourceReceipt {
                        id: receipt_id.clone(),
                        kind: SourceReceiptKind::Command,
                        locator: format!("command:{name}"),
                        version: "fixture-v1".into(),
                        execution_owner: "command-runner".into(),
                        content_digest: format!("{:x}", Sha256::digest(&output.stdout)),
                    },
                    claim_verification: ClaimVerification {
                        id: format!("verification-{name}"),
                        proposition: request.claim_key.clone(),
                        scope: "fixture".into(),
                        version: "fixture-v1".into(),
                        conclusion,
                        source_receipt_ids: vec![receipt_id],
                        verifier: "independent-test".into(),
                    },
                });
            }
            Ok(vec![InvestigationAnswer {
                seat_id: "anonymous-tool".into(),
                new_evidence: items,
                conclusion: FusionClaimPosition::Supports,
            }])
        })
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Keep all three provider faults and their seat-invariance assertions in one acceptance scenario"
)]
async fn public_composition_preserves_disputes_across_provider_faults_and_escalates_empty_rounds() {
    for (fault, code) in [
        (Fault::Timeout, "JEV_TIMEOUT"),
        (Fault::Transport, "JEV_TRANSPORT"),
        (Fault::Invalid, "JEV_INVALID_RESPONSE"),
    ] {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let config = OpenJevRemoteConfig::try_new(OpenJevRemoteConfigRequest {
            provider_id: "judge-route".into(),
            endpoint: "https://judge.example.invalid/score".into(),
            model_id: "judge-model".into(),
            max_batch_size: 1,
            devices: vec![JevDevice::Cpu],
            dtypes: vec![JevDtype::Float32],
            timeout: Duration::from_secs(1),
            api_key: None,
        })
        .unwrap();
        let provider = OpenJevRemoteProvider::new(
            config,
            Arc::new(FaultTransport {
                fault,
                inputs: captured.clone(),
            }),
        );
        let judge = Arc::new(RuntimeFusionJudge::new(
            Arc::new(JevRuntime::new(
                vec![Arc::new(provider)],
                JevRuntimeConfig {
                    timeout: Duration::from_millis(20),
                    retries: 0,
                },
            )),
            JevExecutionOptions {
                device: JevDevice::Cpu,
                dtype: JevDtype::Float32,
            },
        ));
        let mut premises = Vec::new();
        for reversed in [false, true] {
            let investigation = Arc::new(Investigation::default());
            let router = MapFusionProviderRouter::new()
                .with("left", Arc::new(Answer("supports")))
                .with("right", Arc::new(Answer("opposes")))
                .with("third", Arc::new(Answer("supports")));
            let mut candidates = ["left", "right", "third"]
                .map(|route| FusionProviderCandidate {
                    id: format!("{}-{route}", if reversed { "renamed" } else { "original" }),
                    provider: route.into(),
                    model: format!("model-{route}"),
                    reasoning_effort: Some("max".into()),
                })
                .to_vec();
            if reversed {
                candidates.reverse();
            }
            let input = build_fusion_input(
                "Check behavior",
                json!({"provider":"hidden", "model":"hidden", "constraint":"retain both sides"}),
                vec![],
                default_claim_output_schema(),
                candidates,
                FusionBudget::default(),
            );
            let (composed, report) = compose_multiround(
                "fault-panel",
                input,
                Arc::new(router),
                investigation.clone(),
                judge.clone(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(investigation.0.load(Ordering::SeqCst), 2);
            assert_eq!(report.stop_reason, FusionStopReason::Unresolvable);
            assert_eq!(report.jev_unavailable.len(), 1);
            assert_eq!(report.jev_unavailable[0].code, code);
            assert!(report.judged.is_empty());
            assert!(composed.answers.len() >= 2);
            let conflict = composed
                .analysis
                .conflicts
                .iter()
                .find(|conflict| conflict.claim_key == "claim:behavior")
                .unwrap();
            assert_eq!(conflict.positions.len(), 2);
            let inputs = captured.lock().unwrap();
            premises.push(inputs.last().unwrap().clone());
        }
        assert_eq!(
            premises[0], premises[1],
            "seat rename/order changed judge input"
        );
        for forbidden in [
            "hidden",
            "model-left",
            "model-right",
            "original-left",
            "renamed-right",
            "voteCount",
        ] {
            assert!(!premises[0].premise.contains(forbidden), "{forbidden}");
        }
    }
}
