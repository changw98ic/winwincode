// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};
use winwincode_session::migration::{
    MigrationCommit, MigrationError, MigrationOutcome, MigrationTransaction,
    MigrationTransactionError, migrate_legacy_delivery_json,
};

#[derive(Default)]
struct RecordingTransaction {
    consumed_sources: HashSet<String>,
    committed_snapshots: HashMap<String, Vec<u8>>,
    commit_count: usize,
    fail_next: bool,
}

impl MigrationTransaction for RecordingTransaction {
    fn commit_once(
        &mut self,
        source_key: &str,
        canonical_snapshot: &[u8],
    ) -> Result<MigrationCommit, MigrationTransactionError> {
        if self.fail_next {
            self.fail_next = false;
            return Err(MigrationTransactionError::Storage {
                message: "simulated transaction crash".to_owned(),
            });
        }
        if self.consumed_sources.contains(source_key) {
            return Ok(MigrationCommit::AlreadyConsumed {
                canonical_snapshot: self
                    .committed_snapshots
                    .get(source_key)
                    .expect("consumed source has its snapshot")
                    .clone(),
            });
        }

        self.commit_count += 1;
        self.consumed_sources.insert(source_key.to_owned());
        self.committed_snapshots
            .insert(source_key.to_owned(), canonical_snapshot.to_vec());
        Ok(MigrationCommit::Applied)
    }
}

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/session-identity-migration")
        .join(name);
    fs::read(path).expect("session identity migration fixture")
}

fn oracle_snapshots() -> Vec<(String, Value)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/oracles/delivery-strongflow-typescript.v1.json");
    let oracle: Value = serde_json::from_slice(&fs::read(path).expect("delivery oracle"))
        .expect("delivery oracle json");
    oracle["scenarios"]
        .as_array()
        .expect("oracle scenarios")
        .iter()
        .map(|scenario| {
            (
                scenario["id"].as_str().expect("scenario id").to_owned(),
                scenario["observation"]["snapshot"].clone(),
            )
        })
        .collect()
}

fn migrate(input: &[u8], transaction: &mut RecordingTransaction) -> Value {
    let outcome = migrate_legacy_delivery_json(input, transaction).expect("legacy migration");
    serde_json::from_slice(outcome_snapshot(&outcome)).expect("canonical delivery json")
}

fn outcome_snapshot(outcome: &MigrationOutcome) -> &[u8] {
    match outcome {
        MigrationOutcome::Applied {
            canonical_snapshot, ..
        }
        | MigrationOutcome::AlreadyConsumed {
            canonical_snapshot, ..
        } => canonical_snapshot,
    }
}

fn bindings(snapshot: &Value) -> &[Value] {
    snapshot["sessionBindings"]
        .as_array()
        .expect("session bindings")
}

fn stage_runs(snapshot: &Value) -> &[Value] {
    snapshot["stageRuns"].as_array().expect("stage runs")
}

fn assert_closed_canonical_binding(binding: &Value) {
    let object = binding.as_object().expect("canonical binding object");
    let expected = [
        "schemaVersion",
        "id",
        "deliveryId",
        "deliveryTaskId",
        "stageRunId",
        "productSessionId",
        "executionJobId",
        "workerSessionId",
        "codexThreadId",
        "workerId",
        "workerInstanceId",
        "leaseId",
        "attempt",
        "fencingToken",
        "sourceProvenance",
        "boundAtMillis",
    ];
    assert_eq!(object.len(), expected.len());
    for field in expected {
        assert!(
            object.contains_key(field),
            "missing canonical field {field}"
        );
    }
    for field in ["dshSessionId", "codexSessionId", "bindingId"] {
        assert!(!object.contains_key(field), "legacy field leaked: {field}");
    }
    assert_eq!(binding["workerSessionId"], Value::Null);
    assert_eq!(binding["codexThreadId"], Value::Null);
    assert_eq!(binding["workerId"], Value::Null);
    assert_eq!(binding["workerInstanceId"], Value::Null);
    assert_eq!(binding["leaseId"], Value::Null);
    assert_eq!(binding["fencingToken"], Value::Null);
    assert_eq!(binding["sourceProvenance"]["kind"], "legacy-migration");
}

#[test]
fn verifier_snapshot_preserves_graph_ids_and_emits_only_observed_facts() {
    let input = fixture("legacy-verifier.json");
    let mut transaction = RecordingTransaction::default();
    let migrated = migrate(&input, &mut transaction);

    assert_eq!(migrated["id"], "dlv_4WZG68T3YFTA5G9Z3YQMW6CT0G");
    assert!(
        !migrated
            .as_object()
            .expect("delivery object")
            .contains_key("migrationVersion")
    );

    let verifier = bindings(&migrated)
        .iter()
        .find(|binding| binding["id"] == "binding-fixture-success-verifier")
        .expect("verifier binding");
    assert_eq!(verifier["stageRunId"], "stage-fixture-success-verifier");
    assert_eq!(verifier["id"], "binding-fixture-success-verifier");
    assert_eq!(
        verifier["productSessionId"],
        "psn_2B6F278A406DD9806B62059B10"
    );
    assert_eq!(verifier["executionJobId"], "job_64175F7A4A8544CEFCCBACE64D");
    assert_closed_canonical_binding(verifier);
}

#[test]
fn human_stage_bindings_are_removed_instead_of_being_fabricated() {
    let input: Value = serde_json::from_slice(&fixture("legacy-human-review.json"))
        .expect("human legacy snapshot");
    let old_human_stage_ids: HashSet<&str> = stage_runs(&input)
        .iter()
        .filter(|run| run["actorType"] == "human")
        .map(|run| run["id"].as_str().expect("human stage id"))
        .collect();
    assert!(!old_human_stage_ids.is_empty());

    let mut transaction = RecordingTransaction::default();
    let migrated = migrate(
        &serde_json::to_vec(&input).expect("legacy snapshot"),
        &mut transaction,
    );
    for binding in bindings(&migrated) {
        assert!(!old_human_stage_ids.contains(binding["stageRunId"].as_str().unwrap()));
        assert_closed_canonical_binding(binding);
    }
    assert!(bindings(&migrated).iter().all(|binding| {
        stage_runs(&migrated)
            .iter()
            .find(|run| run["id"] == binding["stageRunId"])
            .is_some_and(|run| run["actorType"] == "codex")
    }));
}

#[test]
fn all_ten_legacy_oracle_snapshots_migrate_to_closed_shapes() {
    let scenarios = oracle_snapshots();
    assert_eq!(scenarios.len(), 10);

    for (scenario_id, input) in scenarios {
        let mut transaction = RecordingTransaction::default();
        let output = migrate(
            &serde_json::to_vec(&input).expect("legacy oracle snapshot"),
            &mut transaction,
        );
        let spec = output["spec"].as_object().expect("canonical Spec object");
        assert!(
            spec.contains_key("sourceProductSessionId"),
            "{scenario_id} canonical Spec lacks sourceProductSessionId",
        );
        assert_eq!(spec["sourceProductSessionId"], Value::Null, "{scenario_id}");
        let run_ids: HashSet<&str> = stage_runs(&output)
            .iter()
            .map(|run| run["id"].as_str().expect("stage id"))
            .collect();
        let binding_ids: HashSet<&str> = bindings(&output)
            .iter()
            .map(|binding| {
                assert_closed_canonical_binding(binding);
                assert!(run_ids.contains(binding["stageRunId"].as_str().unwrap()));
                binding["id"].as_str().expect("binding id")
            })
            .collect();
        for evidence in output["evidence"].as_array().expect("evidence") {
            assert!(
                run_ids.contains(evidence["stageRunId"].as_str().unwrap()),
                "{scenario_id}"
            );
            assert!(
                binding_ids.contains(evidence["sessionBindingId"].as_str().unwrap()),
                "{scenario_id}"
            );
        }
        assert!(
            output
                .as_object()
                .expect("delivery object")
                .get("migrationVersion")
                .is_none()
        );
    }
}

#[test]
fn migration_commits_once_and_reports_already_consumed() {
    let input = fixture("legacy-verifier.json");
    let mut transaction = RecordingTransaction::default();
    let first = migrate_legacy_delivery_json(&input, &mut transaction).expect("first migration");
    let second = migrate_legacy_delivery_json(&input, &mut transaction);

    assert!(matches!(first, MigrationOutcome::Applied { .. }));
    assert!(matches!(
        &second,
        Ok(MigrationOutcome::AlreadyConsumed { .. })
    ));
    assert_eq!(outcome_snapshot(&first), outcome_snapshot(&second.unwrap()));
    assert_eq!(transaction.commit_count, 1);
    assert_eq!(transaction.consumed_sources.len(), 1);
}

#[test]
fn invalid_input_does_not_call_or_mark_the_transaction() {
    let mut input: Value =
        serde_json::from_slice(&fixture("legacy-verifier.json")).expect("legacy snapshot");
    input["sessionBindings"][0]["productSessionId"] = json!("psn_01J00000000000000000000000");

    let mut transaction = RecordingTransaction::default();
    let result = migrate_legacy_delivery_json(
        &serde_json::to_vec(&input).expect("mixed snapshot"),
        &mut transaction,
    );

    assert!(matches!(
        result,
        Err(MigrationError::MixedIdentityShape { .. })
    ));
    assert_eq!(transaction.commit_count, 0);
    assert!(transaction.consumed_sources.is_empty());
}

#[test]
fn transaction_crash_leaves_marker_unset_and_retry_applies_once() {
    let input = fixture("legacy-verifier.json");
    let mut transaction = RecordingTransaction {
        fail_next: true,
        ..RecordingTransaction::default()
    };

    let failed = migrate_legacy_delivery_json(&input, &mut transaction);
    assert!(matches!(failed, Err(MigrationError::Transaction { .. })));
    assert!(transaction.consumed_sources.is_empty());
    assert!(transaction.committed_snapshots.is_empty());

    let output = migrate_legacy_delivery_json(&input, &mut transaction).expect("retry migration");
    assert!(matches!(output, MigrationOutcome::Applied { .. }));
    assert_eq!(transaction.commit_count, 1);
    assert_eq!(transaction.consumed_sources.len(), 1);
}

#[test]
fn same_source_is_deterministic_and_different_sources_do_not_collide() {
    let first_input = serde_json::to_vec(&oracle_snapshots()[0].1).expect("first snapshot");
    let second_input = serde_json::to_vec(&oracle_snapshots()[5].1).expect("second snapshot");
    let mut first_store = RecordingTransaction::default();
    let mut second_store = RecordingTransaction::default();

    let first = migrate_legacy_delivery_json(&first_input, &mut first_store).expect("first");
    let replay = migrate_legacy_delivery_json(&first_input, &mut second_store).expect("replay");
    let second = migrate_legacy_delivery_json(&second_input, &mut first_store).expect("second");

    assert_eq!(first, replay);
    assert_ne!(first, second);
    assert_eq!(first_store.consumed_sources.len(), 2);
}

#[test]
fn canonical_input_is_rejected_as_a_second_shape() {
    let input = fixture("legacy-verifier.json");
    let mut first_store = RecordingTransaction::default();
    let canonical = migrate_legacy_delivery_json(&input, &mut first_store).expect("migration");
    let mut second_store = RecordingTransaction::default();

    let result = migrate_legacy_delivery_json(outcome_snapshot(&canonical), &mut second_store);
    assert!(matches!(
        result,
        Err(MigrationError::MixedIdentityShape { .. })
    ));
    assert!(second_store.consumed_sources.is_empty());
}

#[test]
fn unknown_fields_malformed_json_and_duplicate_keys_fail_closed() {
    let mut unknown: Value =
        serde_json::from_slice(&fixture("legacy-verifier.json")).expect("legacy snapshot");
    unknown["legacyAlias"] = json!("legacy");
    let mut transaction = RecordingTransaction::default();
    assert!(matches!(
        migrate_legacy_delivery_json(
            &serde_json::to_vec(&unknown).expect("unknown snapshot"),
            &mut transaction,
        ),
        Err(MigrationError::UnknownField { .. })
    ));
    assert!(transaction.consumed_sources.is_empty());

    for input in [br"{".as_slice(), br"null".as_slice(), br"[]".as_slice()] {
        let mut transaction = RecordingTransaction::default();
        assert!(migrate_legacy_delivery_json(input, &mut transaction).is_err());
        assert!(transaction.consumed_sources.is_empty());
    }

    let duplicate = br#"{
        "schemaVersion": 3,
        "id": "dlv_4WZG68T3YFTA5G9Z3YQMW6CT0G",
        "id": "dlv_01J00000000000000000000000"
    }"#;
    let mut transaction = RecordingTransaction::default();
    assert!(matches!(
        migrate_legacy_delivery_json(duplicate, &mut transaction),
        Err(MigrationError::AmbiguousInput { .. })
    ));
    assert!(transaction.consumed_sources.is_empty());
}

#[test]
fn missing_empty_and_wrong_typed_legacy_binding_fields_fail_closed() {
    for field in [
        "boundAtMillis",
        "codexSessionId",
        "deliveryId",
        "dshSessionId",
        "id",
        "schemaVersion",
        "stageRunId",
    ] {
        let mut input: Value =
            serde_json::from_slice(&fixture("legacy-verifier.json")).expect("legacy snapshot");
        input["sessionBindings"][0]
            .as_object_mut()
            .expect("legacy binding")
            .remove(field);
        let mut transaction = RecordingTransaction::default();
        assert!(
            migrate_legacy_delivery_json(
                &serde_json::to_vec(&input).expect("missing field snapshot"),
                &mut transaction,
            )
            .is_err()
        );
        assert!(transaction.consumed_sources.is_empty());
    }

    for (field, replacement) in [
        ("boundAtMillis", json!("not-an-integer")),
        ("codexSessionId", Value::Null),
        ("deliveryId", json!(true)),
        ("dshSessionId", json!("")),
        ("id", json!("")),
        ("schemaVersion", json!(false)),
        ("stageRunId", json!([])),
    ] {
        let mut input: Value =
            serde_json::from_slice(&fixture("legacy-verifier.json")).expect("legacy snapshot");
        input["sessionBindings"][0][field] = replacement;
        let mut transaction = RecordingTransaction::default();
        assert!(
            migrate_legacy_delivery_json(
                &serde_json::to_vec(&input).expect("invalid field snapshot"),
                &mut transaction,
            )
            .is_err()
        );
        assert!(transaction.consumed_sources.is_empty());
    }
}

#[test]
fn unknown_and_mixed_nested_identity_fields_fail_closed() {
    let mut unknown: Value =
        serde_json::from_slice(&fixture("legacy-verifier.json")).expect("legacy snapshot");
    unknown["stageRuns"][0]["legacyAlias"] = json!("unknown");
    let mut transaction = RecordingTransaction::default();
    assert!(matches!(
        migrate_legacy_delivery_json(
            &serde_json::to_vec(&unknown).expect("unknown stage snapshot"),
            &mut transaction,
        ),
        Err(MigrationError::UnknownField { .. })
    ));
    assert!(transaction.consumed_sources.is_empty());

    let mut mixed: Value =
        serde_json::from_slice(&fixture("legacy-verifier.json")).expect("legacy snapshot");
    mixed["sessionBindings"][0]["workerSessionId"] = Value::Null;
    let mut transaction = RecordingTransaction::default();
    assert!(matches!(
        migrate_legacy_delivery_json(
            &serde_json::to_vec(&mixed).expect("mixed binding snapshot"),
            &mut transaction,
        ),
        Err(MigrationError::MixedIdentityShape { .. })
    ));
    assert!(transaction.consumed_sources.is_empty());
}

#[test]
fn cross_object_references_keep_their_original_ids() {
    let input: Value =
        serde_json::from_slice(&fixture("legacy-verifier.json")).expect("legacy snapshot");
    let mut transaction = RecordingTransaction::default();
    let output = migrate(
        &serde_json::to_vec(&input).expect("legacy snapshot"),
        &mut transaction,
    );

    let old_runs: Vec<&Value> = stage_runs(&input).iter().collect();
    let new_runs: Vec<&Value> = stage_runs(&output).iter().collect();
    assert_eq!(
        old_runs
            .iter()
            .map(|run| run["id"].as_str())
            .collect::<Vec<_>>(),
        new_runs
            .iter()
            .map(|run| run["id"].as_str())
            .collect::<Vec<_>>()
    );

    let new_binding_ids: HashSet<&str> = bindings(&output)
        .iter()
        .map(|binding| binding["id"].as_str().expect("binding id"))
        .collect();
    for evidence in output["evidence"].as_array().expect("evidence") {
        assert!(new_binding_ids.contains(evidence["sessionBindingId"].as_str().unwrap()));
    }
    for (old, new) in input["attentionItems"]
        .as_array()
        .expect("old attention")
        .iter()
        .zip(output["attentionItems"].as_array().expect("new attention"))
    {
        assert_eq!(old["stageRunId"], new["stageRunId"]);
    }
}

#[test]
fn evidence_referencing_removed_or_unknown_bindings_fails_before_commit() {
    let mut input: Value =
        serde_json::from_slice(&fixture("legacy-verifier.json")).expect("legacy snapshot");
    input["evidence"][0]["sessionBindingId"] = json!("binding-fixture-plan-review");
    let mut transaction = RecordingTransaction::default();
    assert!(
        migrate_legacy_delivery_json(
            &serde_json::to_vec(&input).expect("human evidence snapshot"),
            &mut transaction,
        )
        .is_err()
    );
    assert!(transaction.consumed_sources.is_empty());

    input["evidence"][0]["sessionBindingId"] = json!("missing-binding");
    let mut transaction = RecordingTransaction::default();
    assert!(
        migrate_legacy_delivery_json(
            &serde_json::to_vec(&input).expect("unknown evidence snapshot"),
            &mut transaction,
        )
        .is_err()
    );
    assert!(transaction.consumed_sources.is_empty());
}

fn assert_no_legacy_fields(value: &Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                assert_no_legacy_fields(value);
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                assert!(
                    !matches!(key.as_str(), "dshSessionId" | "codexSessionId"),
                    "legacy field leaked: {key}"
                );
                assert_no_legacy_fields(value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[test]
fn canonical_output_contains_no_legacy_identity_keys() {
    let mut transaction = RecordingTransaction::default();
    let output = migrate(&fixture("legacy-verifier.json"), &mut transaction);
    assert_no_legacy_fields(&output);
}
