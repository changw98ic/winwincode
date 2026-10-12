// SPDX-License-Identifier: Apache-2.0

use serde_json::json;
use winwincode_control_plane::fusion_compose::{
    ComposedFusionAnswer, compose_independent_answers, extract_claims_from_answer,
};

fn answer(reference: &str) -> serde_json::Value {
    json!({"claims":[{"claimKey":"claim:fixture", "summary":"Fixture claim",
        "position":"supports", "evidence":[{"evidenceType":"file","sourceRef":reference}],
        "requiredEvidence":["test"]}]})
}

#[test]
fn control_plane_shared_claim_admission_rejects_invalid_reference() {
    assert!(extract_claims_from_answer("candidate", &answer("file:first\nfile:second")).is_err());
    let valid = extract_claims_from_answer("candidate", &answer("file:fixture.rs")).unwrap();
    assert!(valid.claims[0].evidence[0].verified_conclusion.is_none());
}

#[test]
fn portable_collected_answers_keep_explicit_failure_and_original_evidence() {
    let valid = ComposedFusionAnswer {
        candidate_id: "valid".into(),
        answer: answer("file:fixture.rs"),
    };
    let invalid = ComposedFusionAnswer {
        candidate_id: "invalid".into(),
        answer: answer("file:first\nfile:second"),
    };
    let original = vec![valid.clone(), invalid.clone()];
    assert!(compose_independent_answers(original.clone()).is_err());
    assert_eq!(original[1], invalid);
    let composed = compose_independent_answers(vec![valid.clone()]).unwrap();
    assert_eq!(composed.answers, [valid]);
    assert_eq!(
        composed.claims[0].claims[0].evidence[0].source_ref,
        "file:fixture.rs"
    );
}
