// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};
use winwincode_execution_port::{
    generated::{SnapshotFreezeReceiptMessage, SnapshotFreezeRequestMessage},
    snapshot_freeze::{seal_freeze_receipt, validate_freeze_receipt},
};

fn messages() -> (SnapshotFreezeRequestMessage, SnapshotFreezeReceiptMessage) {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let find = |kind| {
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == kind)
            .unwrap()
            .clone()
    };
    let request = serde_json::from_value(find("snapshot.freeze_request")).unwrap();
    let mut message: SnapshotFreezeReceiptMessage =
        serde_json::from_value(find("snapshot.freeze_receipt")).unwrap();
    message.receipt.validation_seal = seal_freeze_receipt(&message.receipt).unwrap();
    (request, message)
}

#[test]
fn exact_receipt_replays_but_changed_identity_or_code_is_rejected_even_when_resealed() {
    let (request, message) = messages();
    validate_freeze_receipt(&request, &message).unwrap();
    let decoded = serde_json::from_slice(&serde_json::to_vec(&message).unwrap()).unwrap();
    validate_freeze_receipt(&request, &decoded).unwrap();
    for (field, value) in [
        ("requestId", json!(format!("req_{}", "Z".repeat(26)))),
        ("candidateId", json!(format!("cnd_{}", "Z".repeat(26)))),
        ("workRunId", json!(format!("wrn_{}", "Z".repeat(26)))),
        ("workerId", json!(format!("wrk_{}", "Z".repeat(26)))),
        ("workerInstanceId", json!(format!("wki_{}", "Z".repeat(26)))),
        ("leaseId", json!(format!("lse_{}", "Z".repeat(26)))),
        ("repositoryId", json!(format!("rep_{}", "Z".repeat(26)))),
        ("attempt", json!(2)),
        ("fencingToken", json!("999")),
        ("baseCommitId", json!("e".repeat(40))),
        ("baseTreeId", json!("e".repeat(40))),
        ("candidateCommitId", json!("e".repeat(40))),
        ("candidateTreeId", json!("e".repeat(40))),
        ("diffSha256", json!(format!("sha256:{}", "e".repeat(64)))),
        ("contentDigest", json!(format!("sha256:{}", "e".repeat(64)))),
    ] {
        let mut changed = serde_json::to_value(&message).unwrap();
        changed["receipt"][field] = value;
        let mut changed: SnapshotFreezeReceiptMessage = serde_json::from_value(changed).unwrap();
        changed.receipt.validation_seal = seal_freeze_receipt(&changed.receipt).unwrap();
        assert!(
            validate_freeze_receipt(&request, &changed).is_err(),
            "{field}"
        );
    }
    let mut changed = message.clone();
    changed.lease.job_id.0.push('X');
    assert!(validate_freeze_receipt(&request, &changed).is_err());
    let mut changed = message;
    changed.receipt.frozen_at.0.push('X');
    assert!(validate_freeze_receipt(&request, &changed).is_err());
}

#[test]
fn verification_retry_does_not_change_the_candidate_producer_attempt() {
    let (mut request, mut message) = messages();
    let producer_attempt = request.candidate.attempt;
    request.lease.attempt = producer_attempt + 1;
    request.dispatch.lease = request.lease.clone();
    request.dispatch.job.attempt = request.lease.attempt;
    if let winwincode_execution_port::generated::ExecutionScope::WorkRunExecutionScope(scope) =
        &mut request.dispatch.job.scope
    {
        scope.attempt = request.lease.attempt;
    }
    message.lease = request.lease.clone();
    message.receipt.attempt = request.lease.attempt;
    message.receipt.validation_seal = seal_freeze_receipt(&message.receipt).unwrap();
    validate_freeze_receipt(&request, &message).unwrap();
    assert_eq!(request.candidate.attempt, producer_attempt);

    message.receipt.attempt = producer_attempt;
    message.receipt.validation_seal = seal_freeze_receipt(&message.receipt).unwrap();
    assert!(validate_freeze_receipt(&request, &message).is_err());
}
