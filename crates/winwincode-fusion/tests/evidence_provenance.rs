// SPDX-License-Identifier: Apache-2.0

use winwincode_fusion::evidence::{
    ClaimVerification, EvidenceLedger, ModelClaim, SourceReceipt, SourceReceiptKind,
    VerificationConclusion,
};

fn receipt(id: &str, digest: &str) -> SourceReceipt {
    SourceReceipt {
        id: id.to_owned(),
        kind: SourceReceiptKind::Test,
        locator: format!("test:{id}"),
        version: "source-digest-1".to_owned(),
        execution_owner: "worker-run-42".to_owned(),
        content_digest: digest.to_owned(),
    }
}

fn claim(claim_id: &str, receipt_ids: Vec<String>) -> ModelClaim {
    ModelClaim {
        claim_id: claim_id.to_owned(),
        proposition: "the heartbeat cannot regress".to_owned(),
        scope: "attempt lifecycle".to_owned(),
        version: "source-digest-1".to_owned(),
        explanation: "The test output supports this proposition".to_owned(),
        source_receipt_ids: receipt_ids,
    }
}

#[test]
fn prose_only_model_claim_is_preserved_but_is_not_evidence() {
    let mut ledger = EvidenceLedger::default();

    let admission = ledger.admit_model_claim(ModelClaim {
        claim_id: "claim-1".to_owned(),
        proposition: "the heartbeat cannot regress".to_owned(),
        scope: "attempt lifecycle".to_owned(),
        version: "source-digest-1".to_owned(),
        explanation: "I believe the terminal write is monotonic".to_owned(),
        source_receipt_ids: Vec::new(),
    });

    assert!(!admission.produced_new_evidence);
    assert_eq!(
        admission.model_explanation,
        "I believe the terminal write is monotonic"
    );
}

#[test]
fn source_receipt_binds_its_version_and_execution_owner_without_proving_the_claim() {
    let mut ledger = EvidenceLedger::default();
    let receipt = ledger
        .register_source_receipt(receipt("receipt-command-1", &"1f".repeat(32)))
        .expect("machine receipt registers");

    let admission = ledger.admit_model_claim(claim("claim-1", vec![receipt.id]));

    assert_eq!(
        admission.resolved_source_receipt_ids,
        vec!["receipt-command-1"]
    );
    assert!(!admission.produced_new_evidence);
}

#[test]
fn source_receipt_requires_version_execution_owner_and_content_digest() {
    let mut ledger = EvidenceLedger::default();
    let mut incomplete = receipt("receipt-1", &"1f".repeat(32));
    incomplete.execution_owner.clear();
    assert!(ledger.register_source_receipt(incomplete).is_err());

    let mut malformed = receipt("receipt-2", "not-a-digest");
    malformed.version.clear();
    assert!(ledger.register_source_receipt(malformed).is_err());
}

#[test]
fn verified_claim_evidence_counts_once_even_when_receipt_ids_and_labels_change() {
    let mut ledger = EvidenceLedger::default();
    let first = ledger
        .register_source_receipt(receipt("receipt-a", &"2b".repeat(32)))
        .unwrap();
    ledger
        .record_claim_verification(ClaimVerification {
            id: "verification-a".to_owned(),
            proposition: "the heartbeat cannot regress".to_owned(),
            scope: "attempt lifecycle".to_owned(),
            version: "source-digest-1".to_owned(),
            conclusion: VerificationConclusion::Supports,
            source_receipt_ids: vec![first.id.clone()],
            verifier: "independent-verifier".to_owned(),
        })
        .unwrap();

    let initial = ledger.admit_model_claim(claim("claim-a", vec![first.id]));
    assert!(initial.produced_new_evidence);

    let duplicate = ledger
        .register_source_receipt(SourceReceipt {
            id: "receipt-relabelled".to_owned(),
            kind: SourceReceiptKind::Command,
            locator: "command:the-same-output".to_owned(),
            version: "source-digest-1".to_owned(),
            execution_owner: "worker-run-99".to_owned(),
            content_digest: "2b".repeat(32),
        })
        .unwrap();
    ledger
        .record_claim_verification(ClaimVerification {
            id: "verification-relabelled".to_owned(),
            proposition: "the heartbeat cannot regress".to_owned(),
            scope: "attempt lifecycle".to_owned(),
            version: "source-digest-1".to_owned(),
            conclusion: VerificationConclusion::Counter,
            source_receipt_ids: vec![duplicate.id.clone()],
            verifier: "another-independent-verifier".to_owned(),
        })
        .unwrap();
    let repeated = ledger.admit_model_claim(claim("claim-a-relabelled", vec![duplicate.id]));
    assert!(!repeated.produced_new_evidence);
}

#[test]
fn counter_refutes_only_the_same_proposition_scope_and_version() {
    let mut ledger = EvidenceLedger::default();
    let receipt = ledger
        .register_source_receipt(receipt("counter-receipt", &"3c".repeat(32)))
        .unwrap();
    ledger
        .record_claim_verification(ClaimVerification {
            id: "counter-verification".to_owned(),
            proposition: "the heartbeat cannot regress".to_owned(),
            scope: "attempt lifecycle".to_owned(),
            version: "source-digest-1".to_owned(),
            conclusion: VerificationConclusion::Counter,
            source_receipt_ids: vec![receipt.id],
            verifier: "independent-verifier".to_owned(),
        })
        .unwrap();

    let exact = claim("claim-exact", Vec::new());
    assert!(ledger.has_valid_counter(&exact));
    assert!(ledger.can_refute(&exact));

    let wrong_scope = ModelClaim {
        scope: "different lifecycle".to_owned(),
        ..exact
    };
    assert!(!ledger.has_valid_counter(&wrong_scope));
    assert!(!ledger.can_refute(&wrong_scope));
}

#[test]
fn invalidating_a_source_receipt_revokes_derived_support_and_counter_status() {
    let mut ledger = EvidenceLedger::default();
    let receipt = ledger
        .register_source_receipt(receipt("receipt-valid", &"4d".repeat(32)))
        .unwrap();
    ledger
        .record_claim_verification(ClaimVerification {
            id: "verification-valid".to_owned(),
            proposition: "the heartbeat cannot regress".to_owned(),
            scope: "attempt lifecycle".to_owned(),
            version: "source-digest-1".to_owned(),
            conclusion: VerificationConclusion::Counter,
            source_receipt_ids: vec![receipt.id.clone()],
            verifier: "independent-verifier".to_owned(),
        })
        .unwrap();

    let target = claim("claim-invalidation", vec![receipt.id.clone()]);
    assert!(ledger.has_valid_counter(&target));
    assert!(ledger.invalidate_source_receipt(&receipt.id));
    assert!(!ledger.has_valid_counter(&target));
    assert!(!ledger.can_refute(&target));
}
