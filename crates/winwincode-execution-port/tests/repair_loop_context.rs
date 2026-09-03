// SPDX-License-Identifier: Apache-2.0

use serde_json::json;
use winwincode_domain::Sha256Digest;
use winwincode_execution_port::generated::{
    ChangeBatchReceiptStatus, FinalCandidateFreezeFact, RepairLoopBudget, RepairLoopContextPack,
    RepairLoopCounters,
};
use winwincode_execution_port::repair_loop_context::{
    FinalCandidateFreezeError, RepairLoopBoundsError, RepairLoopContextError,
    derive_repair_loop_context_digest, seal_repair_loop_context_pack,
    validate_final_candidate_freeze_fact, validate_repair_loop_budget,
    validate_repair_loop_context_pack, validate_repair_loop_counters,
};

fn digest(character: char) -> String {
    format!("sha256:{}", character.to_string().repeat(64))
}

fn sample_pack() -> RepairLoopContextPack {
    let identity = json!({
        "batchId": digest('0'),
        "runKey": "run-key-1",
        "jobId": "job_00000000000000000000000000",
        "attempt": 1,
        "leaseId": "lse_00000000000000000000000000",
        "fencingToken": "1",
        "sessionIdentity": {
            "productSessionId": "psn_00000000000000000000000000",
            "workerSessionId": "wsn_00000000000000000000000000",
            "codexThreadId": "cdx_00000000000000000000000000"
        },
        "repositoryId": "rep_00000000000000000000000000",
        "workspaceRevision": format!("git-tree:{}", "0".repeat(40)),
        "turnId": "turn-1",
        "patchDigest": digest('1')
    });
    let observed_revision = format!("git-tree:{}", "f".repeat(40));
    serde_json::from_value(json!({
        "schemaVersion": 1,
        "identity": identity.clone(),
        "observedRevision": observed_revision.clone(),
        "proposalDisposition": "continue",
        "contextDigest": digest('2'),
        "serializedByteCount": 1,
        "goalSummary": "Complete the exact bounded change.",
        "completedAcceptanceCriteria": [{
            "id": "criterion-complete",
            "summary": "The completed check remains green."
        }],
        "incompleteAcceptanceCriteria": [{
            "id": "criterion-next",
            "summary": "The remaining bounded check must pass."
        }],
        "repairEnvelope": null,
        "latestReceipt": {
            "identity": identity,
            "status": "applied",
            "baseRevision": format!("git-tree:{}", "0".repeat(40)),
            "resultRevision": observed_revision,
            "deltaDigest": digest('3'),
            "deltaExact": true,
            "files": [{
                "path": "src/lib.rs",
                "operation": "update",
                "beforeSha256": digest('4'),
                "afterSha256": digest('5'),
                "bytesBefore": 10,
                "bytesAfter": 12,
                "modeBefore": "0644",
                "modeAfter": "0644"
            }],
            "normalizer": null,
            "validation": null,
            "observation": null,
            "artifactRef": null
        },
        "latestObservation": null,
        "artifactRefs": []
    }))
    .expect("sample context obeys the generated schema")
}

fn sample_final_fact() -> FinalCandidateFreezeFact {
    let (pack, _) = seal_repair_loop_context_pack(sample_pack()).expect("context seals");
    serde_json::from_value(json!({
        "schemaVersion": 1,
        "identity": pack.identity.clone(),
        "resultRevision": pack.observed_revision.clone(),
        "deltaDigest": pack.latest_receipt.delta_digest.clone(),
        "finalReceipt": pack.latest_receipt.clone(),
        "finalObservation": null,
        "counters": {
            "repairRounds": 0,
            "observerCalls": 0,
            "primaryModelCalls": 1,
            "totalTokens": 100,
            "totalCostMicrounits": 25,
            "elapsedMillis": 1000,
            "changeBatches": 1,
            "contextPackBytes": pack.serialized_byte_count
        },
        "stopReason": "accepted",
        "contextPackDigest": pack.context_digest,
        "candidateArtifactRef": {
            "artifactId": "art_00000000000000000000000000",
            "digest": digest('9')
        },
        "frozenAt": "2026-09-02T00:00:00.000Z"
    }))
    .expect("sample final fact obeys the generated schema")
}

fn sample_budget() -> RepairLoopBudget {
    serde_json::from_value(json!({
        "maxRepairRounds": 3,
        "maxObserverCalls": 4,
        "maxPrimaryModelCalls": 8,
        "maxTotalTokens": 10_000_000,
        "maxTotalCostMicrounits": 9_007_199_254_740_991_i64,
        "maxWallTimeMillis": 3_600_000,
        "maxChangeBatches": 4,
        "maxContextPackBytes": 131_072
    }))
    .expect("sample budget obeys the generated schema")
}

fn sample_counters() -> RepairLoopCounters {
    sample_final_fact().counters
}

#[test]
fn seal_derives_digest_and_exact_payload_length_without_observer_history() {
    let (pack, payload) = seal_repair_loop_context_pack(sample_pack()).expect("context seals");

    assert_eq!(
        usize::try_from(pack.serialized_byte_count).expect("positive byte count"),
        payload.len()
    );
    assert_eq!(
        pack.context_digest,
        derive_repair_loop_context_digest(&pack).expect("digest derives")
    );
    assert!(pack.latest_observation.is_none());
    validate_repair_loop_context_pack(&pack, &payload).expect("sealed payload validates");
}

#[test]
fn validation_rejects_caller_byte_count_digest_and_oversized_payload() {
    let (pack, payload) = seal_repair_loop_context_pack(sample_pack()).expect("context seals");

    let mut wrong_count = pack.clone();
    wrong_count.serialized_byte_count += 1;
    assert_eq!(
        validate_repair_loop_context_pack(&wrong_count, &payload),
        Err(RepairLoopContextError::SerializedByteCountMismatch)
    );

    let mut wrong_digest = pack.clone();
    wrong_digest.context_digest = Sha256Digest(digest('e'));
    let wrong_digest_payload = serde_json::to_vec(&wrong_digest).expect("serializes");
    assert_eq!(wrong_digest_payload.len(), payload.len());
    assert_eq!(
        validate_repair_loop_context_pack(&wrong_digest, &wrong_digest_payload),
        Err(RepairLoopContextError::DigestMismatch)
    );

    assert_eq!(
        validate_repair_loop_context_pack(&pack, &vec![b'x'; 131_073]),
        Err(RepairLoopContextError::PayloadTooLarge)
    );
}

#[test]
fn sealing_rejects_empty_duplicate_or_cross_identity_context() {
    let mut wrong_version = sample_pack();
    wrong_version.schema_version = 2;
    assert_eq!(
        seal_repair_loop_context_pack(wrong_version),
        Err(RepairLoopContextError::InvalidSchemaVersion)
    );

    let mut multiline_goal = sample_pack();
    multiline_goal.goal_summary = "first line\nsecond line".to_owned();
    assert_eq!(
        seal_repair_loop_context_pack(multiline_goal),
        Err(RepairLoopContextError::InvalidGoalSummary)
    );

    let mut missing = sample_pack();
    missing.completed_acceptance_criteria.clear();
    missing.incomplete_acceptance_criteria.clear();
    assert_eq!(
        seal_repair_loop_context_pack(missing),
        Err(RepairLoopContextError::MissingAcceptanceCriteria)
    );

    let mut duplicate = sample_pack();
    duplicate.incomplete_acceptance_criteria[0].id =
        duplicate.completed_acceptance_criteria[0].id.clone();
    assert_eq!(
        seal_repair_loop_context_pack(duplicate),
        Err(RepairLoopContextError::DuplicateAcceptanceCriterion)
    );

    let mut oversized_partition = sample_pack();
    oversized_partition.completed_acceptance_criteria = (0..65)
        .map(|index| {
            let mut criterion = oversized_partition.completed_acceptance_criteria[0].clone();
            criterion.id = format!("criterion-{index}");
            criterion
        })
        .collect();
    assert_eq!(
        seal_repair_loop_context_pack(oversized_partition),
        Err(RepairLoopContextError::AcceptanceCriteriaLimitExceeded)
    );

    let mut mismatched = sample_pack();
    mismatched.latest_receipt.identity.turn_id = "another-turn".to_owned();
    assert_eq!(
        seal_repair_loop_context_pack(mismatched),
        Err(RepairLoopContextError::IdentityMismatch)
    );

    let mut rejected = sample_pack();
    rejected.latest_receipt.status = ChangeBatchReceiptStatus::Rejected;
    rejected.latest_receipt.files.clear();
    assert_eq!(
        seal_repair_loop_context_pack(rejected),
        Err(RepairLoopContextError::InexactLatestReceipt)
    );
}

#[test]
fn final_freeze_accepts_zero_observer_and_rejects_mismatched_or_unbounded_facts() {
    let fact = sample_final_fact();
    validate_final_candidate_freeze_fact(&fact).expect("hard-check final fact validates");
    assert!(fact.final_observation.is_none());
    assert_eq!(fact.counters.observer_calls, 0);

    let mut wrong_identity = fact.clone();
    wrong_identity.final_receipt.identity.turn_id = "another-turn".to_owned();
    assert_eq!(
        validate_final_candidate_freeze_fact(&wrong_identity),
        Err(FinalCandidateFreezeError::IdentityMismatch)
    );

    let mut wrong_delta = fact.clone();
    wrong_delta.delta_digest = Sha256Digest(digest('8'));
    assert_eq!(
        validate_final_candidate_freeze_fact(&wrong_delta),
        Err(FinalCandidateFreezeError::DeltaMismatch)
    );

    let mut excessive_cost = fact;
    excessive_cost.counters.total_cost_microunits = 9_007_199_254_740_992;
    assert_eq!(
        validate_final_candidate_freeze_fact(&excessive_cost),
        Err(FinalCandidateFreezeError::CounterLimitExceeded)
    );
}

#[test]
fn generated_rust_requires_explicit_nullable_observations_and_both_criterion_partitions() {
    let mut context = serde_json::to_value(sample_pack()).expect("context serializes");
    let object = context.as_object_mut().expect("context object");
    object.remove("latestObservation");
    assert!(serde_json::from_value::<RepairLoopContextPack>(context).is_err());

    let mut context = serde_json::to_value(sample_pack()).expect("context serializes");
    let object = context.as_object_mut().expect("context object");
    object.remove("completedAcceptanceCriteria");
    assert!(serde_json::from_value::<RepairLoopContextPack>(context).is_err());

    let mut final_fact = serde_json::to_value(sample_final_fact()).expect("final fact serializes");
    final_fact
        .as_object_mut()
        .expect("final fact object")
        .remove("finalObservation");
    assert!(serde_json::from_value::<FinalCandidateFreezeFact>(final_fact).is_err());
}

#[test]
fn directly_constructed_budgets_and_counters_obey_canonical_hard_bounds() {
    let budget = sample_budget();
    validate_repair_loop_budget(&budget).expect("canonical budget validates");
    validate_repair_loop_counters(&sample_counters()).expect("canonical counters validate");

    let mut excessive_primary_calls = budget.clone();
    excessive_primary_calls.max_primary_model_calls = 9;
    assert_eq!(
        validate_repair_loop_budget(&excessive_primary_calls),
        Err(RepairLoopBoundsError::InvalidBudget)
    );

    let mut zero_observer_budget = budget.clone();
    zero_observer_budget.max_observer_calls = 0;
    assert_eq!(
        validate_repair_loop_budget(&zero_observer_budget),
        Err(RepairLoopBoundsError::InvalidBudget)
    );

    let mut short_wall_time = budget;
    short_wall_time.max_wall_time_millis = 999;
    assert_eq!(
        validate_repair_loop_budget(&short_wall_time),
        Err(RepairLoopBoundsError::InvalidBudget)
    );

    let mut excessive_cost = sample_counters();
    excessive_cost.total_cost_microunits = 9_007_199_254_740_992;
    assert_eq!(
        validate_repair_loop_counters(&excessive_cost),
        Err(RepairLoopBoundsError::InvalidCounters)
    );

    let mut negative_round = sample_counters();
    negative_round.repair_rounds = -1;
    assert_eq!(
        validate_repair_loop_counters(&negative_round),
        Err(RepairLoopBoundsError::InvalidCounters)
    );
}
