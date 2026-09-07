// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ChangeBatchId, CodexThreadId, ExecutionJobId, FencingToken, LeaseId, ObservationId,
    ProductSessionId, RepositoryId, SessionIdentity, Sha256Digest, WorkerSessionId,
    WorkspaceRevision,
};
use winwincode_execution_port::{
    change_batch_identity::derive_change_batch_id,
    generated::{
        ChangeBatchIdentity, ExecutionOutcomeUsage, ObservationAcceptanceCriterion,
        ObservationDataEgressPolicy, ObservationDecision, ObservationDeltaSummary,
        ObservationIntent, ObservationPromptInjectionScan, ObservationPromptInjectionStatus,
        ObservationReasonCode, ObservationReceipt, ObservationRequest, ObservationResponse,
        ObservationSecretScan, ObservationSecretScanStatus, ObservationSnippet, ObservationSource,
        ObservationUntrustedInput, ObservationUntrustedInputTrustLevel, RepairClass,
        ValidationProfileName,
    },
    observation_contract::{
        MAX_OBSERVATION_REQUEST_BYTES, MAX_OBSERVATION_RESPONSE_BYTES,
        ObservationContractErrorCode, derive_observation_content_digest, derive_observation_id,
        derive_observation_input_digest, derive_observation_output_digest,
        derive_observation_profile_digest, observation_response_json_schema,
        parse_observation_request_strict, parse_observation_response_strict,
        validate_observation_intent, validate_observation_receipt, validate_observation_request,
        validate_observation_response,
    },
};

fn digest(fill: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", fill.to_string().repeat(64)))
}

fn identity() -> ChangeBatchIdentity {
    let patch_digest = digest('1');
    ChangeBatchIdentity {
        attempt: 1,
        batch_id: derive_change_batch_id("run-key-1", "turn-1", None, &patch_digest)
            .expect("derive batch identity"),
        call_id: None,
        fencing_token: FencingToken("1".to_owned()),
        job_id: ExecutionJobId("job_00000000000000000000000000".to_owned()),
        lease_id: LeaseId("lse_00000000000000000000000000".to_owned()),
        patch_digest,
        repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
        run_key: "run-key-1".to_owned(),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId("cdx_00000000000000000000000000".to_owned()),
            product_session_id: ProductSessionId("psn_00000000000000000000000000".to_owned()),
            stage_run_id: None,
            worker_session_id: WorkerSessionId("wsn_00000000000000000000000000".to_owned()),
        },
        turn_id: "turn-1".to_owned(),
        workspace_revision: WorkspaceRevision(
            "git-tree:0000000000000000000000000000000000000000".to_owned(),
        ),
    }
}

fn clean_intent() -> ObservationIntent {
    let identity = identity();
    let result_revision =
        WorkspaceRevision("git-tree:ffffffffffffffffffffffffffffffffffffffff".to_owned());
    let profile_digest = derive_observation_profile_digest(
        &ValidationProfileName::Fast,
        &digest('2'),
        &["cargo-check".to_owned()],
    )
    .expect("profile digest");
    let observation_id =
        derive_observation_id(&identity.batch_id, &result_revision, &profile_digest)
            .expect("observation ID");
    let snippet_content = "pub fn answer() -> i32 { 42 }".to_owned();
    let snippet_digest = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(snippet_content.as_bytes())
    ));
    let delta_digest = digest('3');
    let mut untrusted_input = ObservationUntrustedInput {
        acceptance_criteria: vec![ObservationAcceptanceCriterion {
            id: "criterion-1".to_owned(),
            summary: "The requested behavior is present.".to_owned(),
        }],
        batch_summary: "One bounded source update.".to_owned(),
        content_digest: digest('0'),
        delta: ObservationDeltaSummary {
            delta_digest: delta_digest.clone(),
            delta_exact: true,
            file_count: 1,
            hunk_count: 1,
            summary: "One exact source-file delta.".to_owned(),
        },
        failed_tests: Vec::new(),
        goal_summary: "Implement the requested bounded behavior.".to_owned(),
        new_diagnostics: Vec::new(),
        snippets: vec![ObservationSnippet {
            content: snippet_content,
            content_digest: snippet_digest,
            end_line: 1,
            path: "src/lib.rs".to_owned(),
            start_line: 1,
        }],
        trust_level: ObservationUntrustedInputTrustLevel::Untrusted,
    };
    untrusted_input.content_digest =
        derive_observation_content_digest(&untrusted_input).expect("content digest");
    let content_digest = untrusted_input.content_digest.clone();
    let mut intent = ObservationIntent {
        all_checks_executed: true,
        data_egress: ObservationDataEgressPolicy {
            external_artifact_reads_allowed: false,
            network_allowed: false,
            provider_file_uploads_allowed: false,
        },
        delta_digest,
        delta_exact: true,
        hard_check_failed: false,
        identity,
        input_digest: digest('0'),
        observation_id,
        profile_digest,
        prompt_injection_scan: ObservationPromptInjectionScan {
            finding_count: 0,
            input_digest: content_digest.clone(),
            rules_digest: digest('4'),
            scanner_version: "prompt-rules-v1".to_owned(),
            status: ObservationPromptInjectionStatus::Clean,
        },
        result_revision,
        secret_scan: ObservationSecretScan {
            finding_count: 0,
            input_digest: content_digest.clone(),
            output_digest: content_digest,
            scanner_version: "secret-rules-v1".to_owned(),
            status: ObservationSecretScanStatus::Clean,
        },
        untrusted_input,
        validation_profile: ValidationProfileName::Fast,
    };
    intent.input_digest = derive_observation_input_digest(&intent).expect("input digest");
    intent
}

fn accept_response(intent: &ObservationIntent) -> ObservationResponse {
    ObservationResponse {
        confidence_bps: 9_500,
        decision: ObservationDecision::Accept,
        observation_id: intent.observation_id.clone(),
        reason_code: ObservationReasonCode::CriteriaSatisfied,
        repair_class: None,
        root_causes: Vec::new(),
        schema_version: 1,
        summary: "The bounded evidence satisfies the acceptance criterion.".to_owned(),
    }
}

#[test]
fn derivation_is_framed_deterministic_and_bound_to_the_exact_result() {
    let intent = clean_intent();
    assert_eq!(
        intent.observation_id,
        ObservationId(
            "sha256:fd712e9d15a21ce18cad4c6913e29be9ed1ffc5d1838a6e230fbfda51ba2d98a".to_owned(),
        ),
        "the cross-runtime observation identity vector must remain stable",
    );
    assert_eq!(
        intent.observation_id,
        derive_observation_id(
            &intent.identity.batch_id,
            &intent.result_revision,
            &intent.profile_digest,
        )
        .expect("same observation ID")
    );
    assert_ne!(
        intent.observation_id,
        derive_observation_id(
            &intent.identity.batch_id,
            &WorkspaceRevision(format!("git-tree:{}", "e".repeat(40))),
            &intent.profile_digest,
        )
        .expect("changed result observation ID")
    );
    let left = derive_observation_id(
        &ChangeBatchId(format!("sha256:{}", "a".repeat(64))),
        &WorkspaceRevision(format!("git-tree:{}", "b".repeat(40))),
        &digest('c'),
    )
    .expect("left framed ID");
    let right = derive_observation_id(
        &ChangeBatchId(format!("sha256:{}", "a".repeat(64))),
        &WorkspaceRevision(format!("git-tree:{}", "b".repeat(64))),
        &digest('c'),
    )
    .expect("right framed ID");
    assert_ne!(left, right);
}

#[test]
fn request_and_receipt_round_trip_with_exact_authority_and_no_raw_payloads() {
    let intent = clean_intent();
    let request = ObservationRequest {
        intent: intent.clone(),
        one_shot: true,
        schema_version: 1,
    };
    validate_observation_request(&request).expect("valid one-shot request");
    let request_value = serde_json::to_value(&request).expect("request JSON");
    assert_eq!(request_value["schemaVersion"], 1);
    assert_eq!(
        serde_json::from_value::<ObservationRequest>(request_value).expect("strict request"),
        request
    );
    assert_eq!(
        parse_observation_request_strict(
            &serde_json::to_vec(&request).expect("strict request bytes")
        )
        .expect("parse strict request"),
        request
    );
    let duplicate = serde_json::to_string(&request)
        .expect("request string")
        .replace("\"oneShot\":true", "\"oneShot\":true,\"oneShot\":true");
    assert_eq!(
        parse_observation_request_strict(duplicate.as_bytes())
            .expect_err("duplicate request field")
            .code(),
        ObservationContractErrorCode::DuplicateField
    );
    let mut unknown = serde_json::to_value(&request).expect("request value");
    unknown
        .as_object_mut()
        .expect("request object")
        .insert("unknown".to_owned(), Value::Bool(true));
    assert_eq!(
        parse_observation_request_strict(
            &serde_json::to_vec(&unknown).expect("unknown request JSON")
        )
        .expect_err("unknown request field")
        .code(),
        ObservationContractErrorCode::InvalidIntent
    );
    assert_eq!(
        parse_observation_request_strict(&vec![b' '; MAX_OBSERVATION_REQUEST_BYTES + 1])
            .expect_err("oversized request")
            .code(),
        ObservationContractErrorCode::RequestTooLarge
    );

    let response = accept_response(&intent);
    let receipt = ObservationReceipt {
        identity: intent.identity.clone(),
        input_digest: intent.input_digest.clone(),
        model_usage: Some(ExecutionOutcomeUsage {
            cost_microunits: 9,
            runtime_millis: 12,
            tokens: 34,
        }),
        output_digest: derive_observation_output_digest(&response).expect("output digest"),
        profile_digest: intent.profile_digest.clone(),
        response,
        result_revision: intent.result_revision.clone(),
        source: ObservationSource::Model,
    };
    validate_observation_receipt(&receipt, &intent).expect("valid exact receipt");
    let encoded = serde_json::to_vec(&receipt).expect("receipt JSON");
    assert!(!encoded.windows(3).any(|window| window == b"pub"));
    assert!(!encoded.windows(6).any(|window| window == b"secret"));
    let mut changed = receipt.clone();
    changed.output_digest = digest('e');
    assert!(validate_observation_receipt(&changed, &intent).is_err());
    let mut changed = receipt.clone();
    changed.model_usage = None;
    assert!(validate_observation_receipt(&changed, &intent).is_err());
    let mut changed = receipt;
    changed.source = ObservationSource::ObserverRuntime;
    changed.model_usage = None;
    assert!(validate_observation_receipt(&changed, &intent).is_err());
}

#[test]
fn unsafe_or_inexact_intents_fail_closed_before_provider_use() {
    let valid = clean_intent();
    let mut cases = Vec::new();
    let mut changed = valid.clone();
    changed.hard_check_failed = true;
    cases.push(changed);
    let mut changed = valid.clone();
    changed.delta_exact = false;
    cases.push(changed);
    let mut changed = valid.clone();
    changed.all_checks_executed = false;
    cases.push(changed);
    let mut changed = valid.clone();
    changed.data_egress.network_allowed = true;
    cases.push(changed);
    let mut changed = valid.clone();
    changed.secret_scan.status = ObservationSecretScanStatus::Rejected;
    cases.push(changed);
    let mut changed = valid.clone();
    changed.observation_id = ObservationId(format!("sha256:{}", "f".repeat(64)));
    cases.push(changed);
    let mut changed = valid.clone();
    changed.untrusted_input.snippets[0].content.push('!');
    cases.push(changed);
    let mut changed = valid.clone();
    changed.untrusted_input.delta.delta_digest = digest('5');
    changed.untrusted_input.content_digest =
        derive_observation_content_digest(&changed.untrusted_input)
            .expect("changed content digest");
    changed.secret_scan.input_digest = changed.untrusted_input.content_digest.clone();
    changed.secret_scan.output_digest = changed.untrusted_input.content_digest.clone();
    changed.prompt_injection_scan.input_digest = changed.untrusted_input.content_digest.clone();
    changed.input_digest =
        derive_observation_input_digest(&changed).expect("changed exact input digest");
    cases.push(changed);
    let mut changed = valid.clone();
    changed.untrusted_input.snippets[0].path = ".gIt/config".to_owned();
    changed.untrusted_input.content_digest =
        derive_observation_content_digest(&changed.untrusted_input).expect("changed path digest");
    changed.secret_scan.input_digest = changed.untrusted_input.content_digest.clone();
    changed.secret_scan.output_digest = changed.untrusted_input.content_digest.clone();
    changed.prompt_injection_scan.input_digest = changed.untrusted_input.content_digest.clone();
    changed.input_digest =
        derive_observation_input_digest(&changed).expect("changed path input digest");
    cases.push(changed);

    for changed in cases {
        assert!(validate_observation_intent(&changed).is_err());
    }
}

#[test]
fn prompt_injection_suspicion_can_be_observed_but_never_accepted() {
    let mut intent = clean_intent();
    intent.prompt_injection_scan.status = ObservationPromptInjectionStatus::Suspected;
    intent.prompt_injection_scan.finding_count = 1;
    intent.input_digest = derive_observation_input_digest(&intent).expect("suspected input digest");
    validate_observation_intent(&intent).expect("suspected input remains observable");

    let accept = accept_response(&intent);
    assert_eq!(
        validate_observation_response(&accept, &intent)
            .expect_err("suspected injection cannot be accepted")
            .code(),
        ObservationContractErrorCode::InvalidResponse
    );
    let repair = ObservationResponse {
        confidence_bps: 8_000,
        decision: ObservationDecision::SemanticRisk,
        observation_id: intent.observation_id.clone(),
        reason_code: ObservationReasonCode::SemanticRiskDetected,
        repair_class: Some(RepairClass::HumanReview),
        root_causes: vec!["The bounded input contains instruction-like source text.".to_owned()],
        schema_version: 1,
        summary: "The evidence requires review before acceptance.".to_owned(),
    };
    validate_observation_response(&repair, &intent).expect("risk decision remains legal");
}

#[test]
fn strict_response_parser_rejects_duplicates_unknown_fields_and_illegal_states() {
    let intent = clean_intent();
    let response = accept_response(&intent);
    assert_eq!(
        serde_json::to_value(&response).expect("response value")["schemaVersion"],
        1,
    );
    let bytes = serde_json::to_vec(&response).expect("response JSON");
    assert_eq!(
        parse_observation_response_strict(&bytes, &intent).expect("strict response"),
        response
    );

    let duplicate = format!(
        concat!(
            "{{\"schemaVersion\":1,\"observationId\":\"{}\",",
            "\"decision\":\"accept\",\"reasonCode\":\"criteria_satisfied\",",
            "\"summary\":\"first\",\"summary\":\"second\",\"rootCauses\":[],",
            "\"repairClass\":null,\"confidenceBps\":9000}}"
        ),
        intent.observation_id.0
    );
    assert_eq!(
        parse_observation_response_strict(duplicate.as_bytes(), &intent)
            .expect_err("duplicate field")
            .code(),
        ObservationContractErrorCode::DuplicateField
    );
    assert_eq!(
        parse_observation_response_strict(br#"{"unknown":1,"unknown":2}"#, &intent)
            .expect_err("duplicate unknown field")
            .code(),
        ObservationContractErrorCode::DuplicateField
    );

    let mut unknown = serde_json::to_value(&response).expect("response value");
    unknown
        .as_object_mut()
        .expect("response object")
        .insert("unexpected".to_owned(), Value::Bool(true));
    assert_eq!(
        parse_observation_response_strict(
            &serde_json::to_vec(&unknown).expect("unknown JSON"),
            &intent,
        )
        .expect_err("unknown field")
        .code(),
        ObservationContractErrorCode::InvalidResponse
    );
    assert_eq!(
        parse_observation_response_strict(b"{", &intent)
            .expect_err("invalid JSON")
            .code(),
        ObservationContractErrorCode::InvalidJson
    );

    let mut illegal = response;
    illegal.decision = ObservationDecision::RepairRequired;
    assert_eq!(
        validate_observation_response(&illegal, &intent)
            .expect_err("illegal decision state")
            .code(),
        ObservationContractErrorCode::InvalidResponse
    );
    let base = accept_response(&intent);
    let mut invalid_responses = Vec::new();
    let mut changed = base.clone();
    changed.schema_version = 2;
    invalid_responses.push(changed);
    let mut changed = base.clone();
    changed.observation_id = ObservationId(format!("sha256:{}", "e".repeat(64)));
    invalid_responses.push(changed);
    let mut changed = base.clone();
    changed.summary = "x".repeat(501);
    invalid_responses.push(changed);
    let mut changed = base.clone();
    changed.confidence_bps = 10_001;
    invalid_responses.push(changed);
    let mut changed = base;
    changed.root_causes = vec!["duplicate cause".to_owned(), "duplicate cause".to_owned()];
    invalid_responses.push(changed);
    for changed in invalid_responses {
        assert_eq!(
            validate_observation_response(&changed, &intent)
                .expect_err("response boundary drift")
                .code(),
            ObservationContractErrorCode::InvalidResponse
        );
    }
    assert_eq!(
        parse_observation_response_strict(
            &vec![b' '; MAX_OBSERVATION_RESPONSE_BYTES + 1],
            &intent,
        )
        .expect_err("oversized response")
        .code(),
        ObservationContractErrorCode::ResponseTooLarge
    );
}

#[test]
fn provider_projection_uses_only_the_frozen_strict_keyword_subset() {
    fn collect_keywords(schema: &Value, output: &mut Vec<String>) {
        let Some(object) = schema.as_object() else {
            return;
        };
        for (key, value) in object {
            output.push(key.clone());
            if key == "properties" {
                if let Some(properties) = value.as_object() {
                    for property in properties.values() {
                        collect_keywords(property, output);
                    }
                }
            } else if key == "items" {
                collect_keywords(value, output);
            }
        }
    }

    let schema = observation_response_json_schema();
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["schemaVersion"]["enum"],
        serde_json::json!([1])
    );
    let mut keywords = Vec::new();
    collect_keywords(&schema, &mut keywords);
    assert!(keywords.iter().all(|keyword| matches!(
        keyword.as_str(),
        "type" | "additionalProperties" | "required" | "properties" | "items" | "enum"
    )));
    for forbidden in [
        "const",
        "uniqueItems",
        "minLength",
        "maxLength",
        "pattern",
        "$ref",
    ] {
        assert!(!keywords.iter().any(|keyword| keyword == forbidden));
    }
}
