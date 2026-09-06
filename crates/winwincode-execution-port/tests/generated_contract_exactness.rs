// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, from_value};
use winwincode_domain::WorkspaceRevision;
use winwincode_execution_port::generated::{
    ActionEnforcementDecision, ActionPolicyKind, ActionPolicyMode, ApprovalDecisionMessageDecision,
    ApprovalDecisionMessageScope, ChangeBatchIdentity, ChangeBatchProgressEvent,
    ChangeBatchProposal, ChangeBatchProposalEvent, ChangeBatchReceipt, DebugExperiment,
    DebugHypothesisLedger, DebugProbePlan, ExecutionPortMessage, InputResponseMessageStatus,
    JobCancelAckMessageStatus, JobCancelMessageReason, JobDispatchResultMessageStatus,
    JobOutcomeAckMessageStatus, ProbeExecutionEvent, ProbeExecutionIntent, ProbeExecutionReceipt,
    ProbeRoundEvent, ProbeRoundReceipt, RepairEnvelope, RoleSessionPolicy, ValidationReceipt,
    WorkerCapabilitySetPlatform, WorkerHeartbeatAckMessageStatus,
    WorkerRegistrationResultMessageLeaseRecovery, WorkerRegistrationResultMessageStatus,
};

fn valid_messages() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("canonical ExecutionPort fixture");
    fixture["messages"]
        .as_array()
        .expect("ExecutionPort messages")
        .clone()
}

#[test]
fn every_canonical_execution_port_message_round_trips_through_the_shared_crate() {
    let messages = valid_messages();
    assert_eq!(messages.len(), 28);

    for message in messages {
        let kind = message["kind"].as_str().expect("message kind");
        let decoded: ExecutionPortMessage = from_value(message.clone())
            .unwrap_or_else(|error| panic!("{kind} must decode: {error}"));
        let encoded = serde_json::to_value(decoded).expect("ExecutionPort message encoding");
        assert_eq!(encoded, message, "{kind} must preserve its canonical shape");
    }
}

#[test]
fn execution_port_messages_reject_unknown_fields_at_the_shared_boundary() {
    for mut message in valid_messages() {
        let kind = message["kind"].as_str().expect("message kind").to_owned();
        message.as_object_mut().expect("message object").insert(
            "unknownField".to_owned(),
            Value::String("reject".to_owned()),
        );
        assert!(
            from_value::<ExecutionPortMessage>(message).is_err(),
            "{kind} must reject unknown fields"
        );
    }
}

#[test]
fn execution_port_inline_string_enums_reject_unknown_values() {
    let cases = [
        ("worker.register", "capabilities.platform"),
        ("worker.capabilities", "capabilities.platform"),
        ("worker.registration_result", "status"),
        ("worker.registration_result", "leaseRecovery"),
        ("worker.heartbeat_ack", "status"),
        ("job.dispatch_result", "status"),
        ("input.response", "status"),
        ("approval.decision", "decision"),
        ("approval.decision", "scope"),
        ("action.enforcement_request", "policyKind"),
        ("action.enforcement_receipt", "policyKind"),
        ("action.enforcement_receipt", "decision"),
        ("action.enforcement_receipt", "policyMode"),
        ("job.cancel", "reason"),
        ("job.cancel_ack", "status"),
        ("job.outcome_ack", "status"),
    ];

    // Keep the enum types in the public generated surface, rather than allowing
    // an inline schema enum to silently regress to an unconstrained String.
    let _: fn(WorkerCapabilitySetPlatform) = |_| {};
    let _: fn(WorkerRegistrationResultMessageStatus) = |_| {};
    let _: fn(WorkerRegistrationResultMessageLeaseRecovery) = |_| {};
    let _: fn(WorkerHeartbeatAckMessageStatus) = |_| {};
    let _: fn(JobDispatchResultMessageStatus) = |_| {};
    let _: fn(InputResponseMessageStatus) = |_| {};
    let _: fn(ApprovalDecisionMessageDecision) = |_| {};
    let _: fn(ApprovalDecisionMessageScope) = |_| {};
    let _: fn(ActionPolicyKind) = |_| {};
    let _: fn(ActionEnforcementDecision) = |_| {};
    let _: fn(ActionPolicyMode) = |_| {};
    let _: fn(JobCancelMessageReason) = |_| {};
    let _: fn(JobCancelAckMessageStatus) = |_| {};
    let _: fn(JobOutcomeAckMessageStatus) = |_| {};

    let fixture = valid_messages();
    for (kind, field) in cases {
        let mut message = fixture
            .iter()
            .find(|message| message["kind"] == kind)
            .unwrap_or_else(|| panic!("missing {kind} fixture"))
            .clone();
        if field == "capabilities.platform" {
            message["capabilities"]["platform"] = Value::String("unsupported-platform".to_owned());
        } else {
            message[field] = Value::String("unsupported-value".to_owned());
        }
        assert!(
            from_value::<ExecutionPortMessage>(message).is_err(),
            "{kind}.{field} must reject an unknown enum value"
        );
    }
}

fn change_batch_identity() -> Value {
    serde_json::json!({
        "batchId": format!("sha256:{}", "0".repeat(64)),
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
        "workspaceRevision": "git-tree:0000000000000000000000000000000000000000",
        "turnId": "turn-1",
        "patchDigest": format!("sha256:{}", "1".repeat(64))
    })
}

fn applied_file_summary() -> Value {
    serde_json::json!({
        "path": "new.txt",
        "operation": "create",
        "afterSha256": format!("sha256:{}", "9".repeat(64)),
        "bytesBefore": 0,
        "bytesAfter": 4,
        "modeAfter": "0644"
    })
}

#[test]
fn workspace_revision_accepts_only_exact_git_tree_object_ids() {
    for valid in [
        format!("git-tree:{}", "0".repeat(40)),
        format!("git-tree:{}", "f".repeat(64)),
    ] {
        let decoded: WorkspaceRevision =
            from_value(Value::String(valid.clone())).expect("valid Git tree revision");
        assert_eq!(decoded.0, valid);
    }

    for invalid in [
        "HEAD".to_owned(),
        "0123456789abcdef".to_owned(),
        format!("git-commit:{}", "0".repeat(40)),
        format!("git-tree:{}", "A".repeat(40)),
        format!("git-tree:{}", "0".repeat(39)),
        format!("git-tree:{}", "0".repeat(41)),
        format!("git-tree:{}", "0".repeat(63)),
        format!("git-tree:{}", "0".repeat(65)),
    ] {
        assert!(
            from_value::<WorkspaceRevision>(Value::String(invalid.clone())).is_err(),
            "{invalid} must not be accepted as an exact workspace tree"
        );
        let mut identity = change_batch_identity();
        identity["workspaceRevision"] = Value::String(invalid.clone());
        assert!(
            from_value::<ChangeBatchIdentity>(identity).is_err(),
            "nested revision {invalid} must fail closed"
        );
    }

    let mut unknown = change_batch_identity();
    unknown["workspaceRevisionObject"] = Value::String(format!("git-tree:{}", "0".repeat(40)));
    assert!(from_value::<ChangeBatchIdentity>(unknown).is_err());
}

#[test]
fn generated_change_batch_contracts_round_trip_and_reject_unknown_oversized_or_illegal_values() {
    let mut non_digest_identity = change_batch_identity();
    non_digest_identity["batchId"] = Value::String("batch-1".to_owned());
    assert!(from_value::<ChangeBatchIdentity>(non_digest_identity).is_err());

    let proposal = serde_json::json!({
        "schemaVersion": 1,
        "disposition": "final",
        "validationProfile": "fast",
        "patch": "*** Begin Patch\n*** End Patch\n",
        "acceptanceCriteriaIds": ["criterion-1"]
    });
    let decoded: ChangeBatchProposal = from_value(proposal.clone()).expect("bounded proposal");
    assert_eq!(
        serde_json::to_value(decoded).expect("proposal JSON"),
        proposal
    );

    let mut unknown = proposal.clone();
    unknown["rawSource"] = Value::String("not allowed".to_owned());
    assert!(from_value::<ChangeBatchProposal>(unknown).is_err());

    let mut oversized = proposal.clone();
    oversized["patch"] = Value::String("x".repeat(524_289));
    assert!(from_value::<ChangeBatchProposal>(oversized).is_err());

    let mut wrong_version = proposal.clone();
    wrong_version["schemaVersion"] = Value::from(2);
    assert!(from_value::<ChangeBatchProposal>(wrong_version).is_err());

    let mut illegal_state = proposal.clone();
    illegal_state["disposition"] = Value::String("maybe".to_owned());
    assert!(from_value::<ChangeBatchProposal>(illegal_state).is_err());

    let mut invalid_profile = proposal.clone();
    invalid_profile["validationProfile"] = Value::String("not valid".to_owned());
    assert!(from_value::<ChangeBatchProposal>(invalid_profile).is_err());

    let mut invalid_criterion = proposal.clone();
    invalid_criterion["acceptanceCriteriaIds"] = serde_json::json!(["not valid"]);
    assert!(from_value::<ChangeBatchProposal>(invalid_criterion).is_err());

    let mut duplicate_criteria = proposal.clone();
    duplicate_criteria["acceptanceCriteriaIds"] = serde_json::json!(["criterion-1", "criterion-1"]);
    assert!(from_value::<ChangeBatchProposal>(duplicate_criteria).is_err());

    let event = serde_json::json!({
        "identity": change_batch_identity(),
        "proposal": proposal,
        "occurredAt": "2026-08-31T12:00:00.000Z"
    });
    let decoded: ChangeBatchProposalEvent = from_value(event.clone()).expect("proposal event");
    assert_eq!(
        serde_json::to_value(decoded).expect("proposal event JSON"),
        event
    );
}

#[test]
fn generated_role_progress_and_repair_contracts_are_closed_and_bounded() {
    let policy = serde_json::json!({
        "schemaVersion": 2,
        "roleId": "executor",
        "workspaceMode": "candidate-read-only",
        "developerInstructions": "Compose one bounded ChangeBatch proposal.",
        "executionMode": "delegated_batch"
    });
    assert!(from_value::<RoleSessionPolicy>(policy.clone()).is_ok());

    let mut legacy = policy.clone();
    legacy["schemaVersion"] = Value::from(1);
    assert!(from_value::<RoleSessionPolicy>(legacy).is_err());
    let mut unknown = policy;
    unknown["legacyMode"] = Value::Bool(true);
    assert!(from_value::<RoleSessionPolicy>(unknown).is_err());

    let progress = serde_json::json!({
        "identity": change_batch_identity(),
        "sequence": 1,
        "state": "proposed",
        "occurredAt": "2026-08-31T12:00:00.000Z",
        "summary": "proposal retained",
        "artifactRefs": []
    });
    assert!(from_value::<ChangeBatchProgressEvent>(progress.clone()).is_ok());
    let mut invalid_progress = progress;
    invalid_progress["state"] = Value::String("applying".to_owned());
    assert!(from_value::<ChangeBatchProgressEvent>(invalid_progress).is_err());

    let repair = serde_json::json!({
        "identity": change_batch_identity(),
        "repairRound": 1,
        "observedRevision": "git-tree:ffffffffffffffffffffffffffffffffffffffff",
        "deltaDigest": format!("sha256:{}", "2".repeat(64)),
        "reasonCode": "validation-failed",
        "rootCauseSummary": "One bounded validation check failed.",
        "diagnosticDigests": [],
        "snippetArtifactRefs": []
    });
    assert!(from_value::<RepairEnvelope>(repair.clone()).is_ok());
    let mut oversized_repair = repair;
    oversized_repair["rootCauseSummary"] = Value::String("x".repeat(501));
    assert!(from_value::<RepairEnvelope>(oversized_repair).is_err());
}

#[test]
fn generated_receipts_round_trip_without_inline_source_or_logs() {
    let validation = serde_json::json!({
        "profile": "fast",
        "status": "passed",
        "baseRevision": "git-tree:0000000000000000000000000000000000000000",
        "resultRevision": "git-tree:ffffffffffffffffffffffffffffffffffffffff",
        "checks": [{
            "name": "cargo-test",
            "status": "passed",
            "summary": "targeted tests passed"
        }],
        "durationMillis": 42,
        "artifactRefs": []
    });
    let decoded: ValidationReceipt = from_value(validation.clone()).expect("validation receipt");
    assert_eq!(
        serde_json::to_value(decoded).expect("validation receipt JSON"),
        validation
    );

    let receipt = serde_json::json!({
        "identity": change_batch_identity(),
        "status": "applied",
        "baseRevision": "git-tree:0000000000000000000000000000000000000000",
        "resultRevision": "git-tree:ffffffffffffffffffffffffffffffffffffffff",
        "deltaDigest": format!("sha256:{}", "4".repeat(64)),
        "deltaExact": true,
        "files": [applied_file_summary()],
        "normalizer": null,
        "validation": validation,
        "observation": null,
        "artifactRef": null
    });
    let decoded: ChangeBatchReceipt = from_value(receipt.clone()).expect("change batch receipt");
    assert_eq!(
        serde_json::to_value(decoded).expect("change batch receipt JSON"),
        receipt.clone()
    );
    for field in ["normalizer", "validation", "observation", "artifactRef"] {
        let mut missing = receipt.clone();
        missing
            .as_object_mut()
            .expect("receipt object")
            .remove(field);
        assert!(
            from_value::<ChangeBatchReceipt>(missing).is_err(),
            "required nullable {field} must not become an optional field"
        );
    }
}

#[test]
fn change_batch_receipt_statuses_bind_only_proven_tree_results() {
    let receipt = serde_json::json!({
        "identity": change_batch_identity(),
        "status": "applied",
        "baseRevision": format!("git-tree:{}", "0".repeat(40)),
        "resultRevision": format!("git-tree:{}", "f".repeat(40)),
        "deltaDigest": format!("sha256:{}", "4".repeat(64)),
        "deltaExact": true,
        "files": [applied_file_summary()],
        "normalizer": null,
        "validation": null,
        "observation": null,
        "artifactRef": null
    });

    for status in ["applied", "partially_applied"] {
        let mut exact = receipt.clone();
        exact["status"] = Value::String(status.to_owned());
        assert!(from_value::<ChangeBatchReceipt>(exact).is_ok(), "{status}");
    }
    let mut rejected = receipt.clone();
    rejected["status"] = Value::String("rejected".to_owned());
    rejected["files"] = serde_json::json!([]);
    assert!(from_value::<ChangeBatchReceipt>(rejected).is_ok());

    let mut empty_applied = receipt.clone();
    empty_applied["files"] = serde_json::json!([]);
    assert!(from_value::<ChangeBatchReceipt>(empty_applied).is_err());
    let mut nonempty_rejected = receipt.clone();
    nonempty_rejected["status"] = Value::String("rejected".to_owned());
    assert!(from_value::<ChangeBatchReceipt>(nonempty_rejected).is_err());

    for field in ["resultRevision", "deltaDigest"] {
        let mut incomplete = receipt.clone();
        incomplete
            .as_object_mut()
            .expect("receipt object")
            .remove(field);
        assert!(
            from_value::<ChangeBatchReceipt>(incomplete).is_err(),
            "an exact receipt must bind {field}"
        );
    }

    let mut false_exact = receipt.clone();
    false_exact["deltaExact"] = Value::Bool(false);
    assert!(from_value::<ChangeBatchReceipt>(false_exact).is_err());

    let mut uncertain = receipt.clone();
    uncertain["status"] = Value::String("state_uncertain".to_owned());
    uncertain["deltaExact"] = Value::Bool(false);
    uncertain
        .as_object_mut()
        .expect("receipt object")
        .remove("resultRevision");
    uncertain
        .as_object_mut()
        .expect("receipt object")
        .remove("deltaDigest");
    assert!(from_value::<ChangeBatchReceipt>(uncertain.clone()).is_ok());

    for forbidden in ["resultRevision", "deltaDigest"] {
        let mut invalid = uncertain.clone();
        invalid[forbidden] = if forbidden == "resultRevision" {
            Value::String(format!("git-tree:{}", "f".repeat(40)))
        } else {
            Value::String(format!("sha256:{}", "4".repeat(64)))
        };
        assert!(
            from_value::<ChangeBatchReceipt>(invalid).is_err(),
            "an uncertain receipt must not claim {forbidden}"
        );

        let mut explicit_null = uncertain.clone();
        explicit_null[forbidden] = Value::Null;
        assert!(
            from_value::<ChangeBatchReceipt>(explicit_null).is_err(),
            "an absent {forbidden} must not accept explicit null"
        );
    }

    let mut illegal_status = uncertain;
    illegal_status["status"] = Value::String("completed".to_owned());
    assert!(from_value::<ChangeBatchReceipt>(illegal_status).is_err());
}

fn debug_probe_authority() -> Value {
    serde_json::json!({
        "debugSessionId": "dbg_00000000000000000000000000",
        "jobId": "job_00000000000000000000000000",
        "attempt": 1,
        "leaseId": "lse_00000000000000000000000000",
        "fencingToken": "7",
        "sessionIdentity": {
            "productSessionId": "psn_00000000000000000000000000",
            "workerSessionId": "wsn_00000000000000000000000000",
            "codexThreadId": "cdx_00000000000000000000000000"
        },
        "repositoryId": "rep_00000000000000000000000000",
        "roundId": "prn_00000000000000000000000000",
        "workspaceRevision": format!("git-tree:{}", "0".repeat(40)),
        "environmentDigest": format!("sha256:{}", "1".repeat(64))
    })
}

fn debug_probe_identity() -> Value {
    let mut identity = debug_probe_authority();
    let object = identity
        .as_object_mut()
        .expect("debug probe authority object");
    object.insert(
        "probeId".to_owned(),
        Value::String("prb_00000000000000000000000000".to_owned()),
    );
    object.insert(
        "probeExecutionId".to_owned(),
        Value::String(format!("sha256:{}", "7".repeat(64))),
    );
    identity
}

fn probe_spec() -> Value {
    serde_json::json!({
        "probeId": "prb_00000000000000000000000000",
        "probeDefinitionDigest": format!("sha256:{}", "2".repeat(64)),
        "kind": "static_analysis",
        "command": {
            "argv": ["corepack", "pnpm", "typecheck"],
            "workingDirectory": ".",
            "commandArgBytes": 30
        },
        "resources": {
            "workspaceAccess": "read_only",
            "sideEffectClass": "pure_read",
            "paths": ["src/example.ts"],
            "cpuLimitMillis": 10_000,
            "memoryLimitBytes": 134_217_728,
            "portNumbers": [],
            "serviceKeys": [],
            "databaseKeys": [],
            "exclusiveKeys": [],
            "networkAccess": "none"
        },
        "timeoutMillis": 300_000,
        "outputLimitBytes": 1_048_576,
        "required": true,
        "targetHypothesisIds": ["hyp_00000000000000000000000000"]
    })
}

fn debug_probe_plan() -> Value {
    serde_json::json!({
        "schemaVersion": 1,
        "authority": debug_probe_authority(),
        "planDigest": format!("sha256:{}", "3".repeat(64)),
        "probes": [probe_spec()],
        "budget": {
            "probeLimit": 8,
            "parallelProbeLimit": 4,
            "wallTimeLimitMillis": 600_000,
            "totalOutputLimitBytes": 8_388_608,
            "totalCpuLimitMillis": 2_400_000,
            "peakMemoryLimitBytes": 1_073_741_824,
            "totalCommandArgLimitBytes": 262_144,
            "budgetDigest": format!("sha256:{}", "6".repeat(64))
        },
        "completionRule": {
            "kind": "all_terminal",
            "minimumCompletedProbes": 1,
            "minimumSuccessfulProbes": 1,
            "stopOnRequiredProbeFailure": true
        },
        "createdAt": "2026-09-04T08:00:00.000Z"
    })
}

fn probe_execution_receipt() -> Value {
    serde_json::json!({
        "schemaVersion": 1,
        "identity": debug_probe_identity(),
        "planDigest": format!("sha256:{}", "3".repeat(64)),
        "status": "succeeded",
        "exitCode": 0,
        "signal": null,
        "timedOut": false,
        "durationMillis": 1_200,
        "outputBytes": 4_096,
        "outputTruncated": false,
        "artifactRefs": [],
        "error": null,
        "startedAt": "2026-09-04T08:00:01.000Z",
        "finishedAt": "2026-09-04T08:00:02.000Z"
    })
}

#[test]
fn generated_debug_probe_execution_contracts_round_trip() {
    let plan = debug_probe_plan();
    let decoded: DebugProbePlan = from_value(plan.clone()).expect("bounded DebugProbe plan");
    assert_eq!(serde_json::to_value(decoded).expect("plan JSON"), plan);

    let intent = serde_json::json!({
        "schemaVersion": 1,
        "identity": debug_probe_identity(),
        "planDigest": format!("sha256:{}", "3".repeat(64)),
        "spec": probe_spec(),
        "createdAt": "2026-09-04T08:00:00.000Z"
    });
    let decoded: ProbeExecutionIntent = from_value(intent.clone()).expect("probe intent");
    assert_eq!(serde_json::to_value(decoded).expect("intent JSON"), intent);

    let receipt = probe_execution_receipt();
    let decoded: ProbeExecutionReceipt = from_value(receipt.clone()).expect("probe receipt");
    assert_eq!(
        serde_json::to_value(decoded).expect("probe receipt JSON"),
        receipt
    );

    let round_receipt = serde_json::json!({
        "schemaVersion": 1,
        "authority": debug_probe_authority(),
        "planDigest": format!("sha256:{}", "3".repeat(64)),
        "status": "completed",
        "completionReason": "all_probes_terminal",
        "probeReceipts": [probe_execution_receipt()],
        "usage": {
            "probeCount": 1,
            "peakParallelProbes": 1,
            "elapsedMillis": 1_200,
            "totalOutputBytes": 4_096,
            "totalCpuMillis": 800,
            "peakMemoryBytes": 67_108_864,
            "totalCommandArgBytes": 30,
            "budgetDigest": format!("sha256:{}", "6".repeat(64))
        },
        "error": null,
        "startedAt": "2026-09-04T08:00:01.000Z",
        "finishedAt": "2026-09-04T08:00:02.000Z"
    });
    let decoded: ProbeRoundReceipt =
        from_value(round_receipt.clone()).expect("probe round receipt");
    assert_eq!(
        serde_json::to_value(decoded).expect("round receipt JSON"),
        round_receipt
    );

    let execution_event = serde_json::json!({
        "identity": debug_probe_identity(),
        "sequence": 1,
        "kind": "finished",
        "status": "succeeded",
        "occurredAt": "2026-09-04T08:00:02.000Z",
        "summary": "probe finished",
        "artifactRefs": []
    });
    assert!(from_value::<ProbeExecutionEvent>(execution_event).is_ok());
    let round_event = serde_json::json!({
        "authority": debug_probe_authority(),
        "sequence": 2,
        "kind": "finished",
        "status": "completed",
        "occurredAt": "2026-09-04T08:00:02.000Z",
        "summary": "round finished"
    });
    assert!(from_value::<ProbeRoundEvent>(round_event).is_ok());
}

#[test]
fn generated_debug_probe_state_contracts_round_trip() {
    let ledger = serde_json::json!({
        "schemaVersion": 1,
        "authority": debug_probe_authority(),
        "sessionStatus": "active",
        "ledgerDigest": format!("sha256:{}", "4".repeat(64)),
        "hypotheses": [{
            "hypothesisId": "hyp_00000000000000000000000000",
            "summary": "The type graph contains a missing edge.",
            "status": "active",
            "confidenceBps": 5_000,
            "supportingEvidence": [],
            "contradictingEvidence": [],
            "lastUpdatedRoundId": "prn_00000000000000000000000000"
        }],
        "confirmedFacts": [],
        "reproductionRecipe": ["Run the bounded typecheck probe."],
        "unresolvedQuestions": ["Which declaration owns the missing edge?"],
        "updatedAt": "2026-09-04T08:00:02.000Z"
    });
    let decoded: DebugHypothesisLedger = from_value(ledger.clone()).expect("hypothesis ledger");
    assert_eq!(serde_json::to_value(decoded).expect("ledger JSON"), ledger);

    let experiment = serde_json::json!({
        "schemaVersion": 1,
        "identity": debug_probe_identity(),
        "experimentId": "exp_00000000000000000000000000",
        "instrumentationDigest": format!("sha256:{}", "5".repeat(64)),
        "baseRevision": format!("git-tree:{}", "0".repeat(40)),
        "experimentRevision": null,
        "status": "planned",
        "createdAt": "2026-09-04T08:00:00.000Z",
        "expiresAt": "2026-09-04T08:10:00.000Z",
        "cleanupReceipt": null
    });
    let decoded: DebugExperiment = from_value(experiment.clone()).expect("debug experiment");
    assert_eq!(
        serde_json::to_value(decoded).expect("experiment JSON"),
        experiment
    );
}

#[test]
fn generated_debug_probe_contracts_fail_closed() {
    let mut unknown = debug_probe_plan();
    unknown["rawLog"] = Value::String("not allowed".to_owned());
    assert!(from_value::<DebugProbePlan>(unknown).is_err());

    let mut empty_job_id = debug_probe_plan();
    empty_job_id["authority"]["jobId"] = Value::String(String::new());
    assert!(from_value::<DebugProbePlan>(empty_job_id).is_err());

    let mut wrong_version = debug_probe_plan();
    wrong_version["schemaVersion"] = Value::from(2);
    assert!(from_value::<DebugProbePlan>(wrong_version).is_err());

    let mut too_many_probes = debug_probe_plan();
    too_many_probes["probes"] = Value::Array((0..33).map(|_| probe_spec()).collect());
    assert!(from_value::<DebugProbePlan>(too_many_probes).is_err());

    let mut too_many_arguments = debug_probe_plan();
    too_many_arguments["probes"][0]["command"]["argv"] =
        Value::Array((0..65).map(|_| Value::String("x".to_owned())).collect());
    assert!(from_value::<DebugProbePlan>(too_many_arguments).is_err());

    let mut too_many_paths = debug_probe_plan();
    too_many_paths["probes"][0]["resources"]["paths"] = Value::Array(
        (0..129)
            .map(|index| Value::String(format!("src/{index}.rs")))
            .collect(),
    );
    assert!(from_value::<DebugProbePlan>(too_many_paths).is_err());

    for (field, invalid) in [("timeoutMillis", 600_001), ("outputLimitBytes", 16_777_217)] {
        let mut over_limit = debug_probe_plan();
        over_limit["probes"][0][field] = Value::from(invalid);
        assert!(from_value::<DebugProbePlan>(over_limit).is_err(), "{field}");
    }

    for (field, invalid) in [
        ("cpuLimitMillis", 3_600_001_i64),
        ("memoryLimitBytes", 8_589_934_593_i64),
    ] {
        let mut over_limit = debug_probe_plan();
        over_limit["probes"][0]["resources"][field] = Value::from(invalid);
        assert!(from_value::<DebugProbePlan>(over_limit).is_err(), "{field}");
    }

    let mut write_authority = debug_probe_plan();
    write_authority["probes"][0]["resources"]["workspaceAccess"] =
        Value::String("candidate_write".to_owned());
    assert!(from_value::<DebugProbePlan>(write_authority).is_err());

    let mut over_budget = debug_probe_plan();
    over_budget["budget"]["totalOutputLimitBytes"] = Value::from(268_435_457);
    assert!(from_value::<DebugProbePlan>(over_budget).is_err());

    let mut missing_nullable = probe_execution_receipt();
    missing_nullable
        .as_object_mut()
        .expect("probe receipt object")
        .remove("error");
    assert!(from_value::<ProbeExecutionReceipt>(missing_nullable).is_err());

    let mut zero_sequence = serde_json::json!({
        "authority": debug_probe_authority(),
        "sequence": 0,
        "kind": "started",
        "status": "running",
        "occurredAt": "2026-09-04T08:00:01.000Z",
        "summary": "round started"
    });
    assert!(from_value::<ProbeRoundEvent>(zero_sequence.clone()).is_err());
    zero_sequence["sequence"] = Value::from(1);
    zero_sequence["status"] = Value::String("unknown".to_owned());
    assert!(from_value::<ProbeRoundEvent>(zero_sequence).is_err());

    let read_only_policy = serde_json::json!({
        "schemaVersion": 2,
        "roleId": "executor",
        "workspaceMode": "candidate-read-only",
        "developerInstructions": "Produce one bounded DebugProbe plan.",
        "executionMode": "debug_probe"
    });
    assert!(from_value::<RoleSessionPolicy>(read_only_policy.clone()).is_ok());
    let mut writable_policy = read_only_policy;
    writable_policy["workspaceMode"] = Value::String("candidate-write".to_owned());
    assert!(from_value::<RoleSessionPolicy>(writable_policy).is_err());
}
