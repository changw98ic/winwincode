// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};
use winwincode_fusion::{analysis::analyze_fusion, claims::extract_claims_from_answer};

fn answer() -> Value {
    json!({"claims":[{"claimKey":"claim:fixture", "summary":"Fixture claim",
        "position":"supports", "evidence":[{"evidenceType":"file","sourceRef":"file:fixture.rs"}],
        "requiredEvidence":["test"]}]})
}

#[test]
fn shared_claim_admission_rejects_internal_reference_line_feed() {
    let mut input = answer();
    input["claims"][0]["evidence"][0]["sourceRef"] = json!("file:first\nfile:second");
    let extracted = extract_claims_from_answer("candidate", &input);
    if let Ok(candidate) = &extracted {
        assert!(analyze_fusion(std::slice::from_ref(candidate)).is_err());
    }
    assert!(
        extracted.is_err(),
        "member admission must enforce aggregate reference constraints"
    );
}

#[test]
fn shared_claim_admission_rejects_controls_and_size_limits() {
    for (path, value) in [
        ("claimKey", "claim:bad\tkey".to_owned()),
        ("summary", "summary\u{7f}bad".to_owned()),
        ("claimKey", "k".repeat(4097)),
        ("summary", "s".repeat(65537)),
    ] {
        let mut input = answer();
        input["claims"][0][path] = json!(value);
        assert!(
            extract_claims_from_answer("candidate", &input).is_err(),
            "invalid {path}"
        );
    }
    let mut input = answer();
    input["claims"][0]["evidence"][0]["sourceRef"] = json!("r".repeat(4097));
    assert!(extract_claims_from_answer("candidate", &input).is_err());
    assert!(extract_claims_from_answer("bad\ncandidate", &answer()).is_err());
}

#[test]
fn shared_claim_admission_rejects_excess_counts_and_normalized_duplicates() {
    let row = answer()["claims"][0].clone();
    assert!(
        extract_claims_from_answer("candidate", &json!({"claims":vec![row.clone();1001]})).is_err()
    );
    let mut input = answer();
    input["claims"][0]["evidence"] = json!(vec![row["evidence"][0].clone(); 1001]);
    assert!(extract_claims_from_answer("candidate", &input).is_err());
    input = answer();
    input["claims"][0]["requiredEvidence"] = json!(vec!["test"; 1001]);
    assert!(extract_claims_from_answer("candidate", &input).is_err());
    let mut duplicate = row.clone();
    duplicate["claimKey"] = json!("Claim:FIXTURE");
    assert!(extract_claims_from_answer("candidate", &json!({"claims":[row,duplicate]})).is_err());
}

#[test]
fn shared_claim_admission_accepts_valid_claims_without_evidence_promotion() {
    let candidate = extract_claims_from_answer("candidate", &answer()).unwrap();
    assert_eq!(candidate.claims.len(), 1);
    assert_eq!(candidate.claims[0].evidence.len(), 1);
    assert!(
        candidate.claims[0].evidence[0]
            .verified_conclusion
            .is_none()
    );
    assert!(analyze_fusion(&[candidate]).is_ok());
}
