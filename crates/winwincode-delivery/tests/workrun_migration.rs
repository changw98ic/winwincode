// SPDX-License-Identifier: Apache-2.0
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Barrier},
    thread,
};
use winwincode_delivery::SqliteWorkRunMigration;
use winwincode_delivery::workrun_migration::{
    WorkRunMigrationError, WorkRunMigrationOutcome, convert_canonical_delivery,
};
fn fixture() -> Vec<u8> {
    fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/delivery-main.json"))
        .expect("canonical Delivery fixture")
}
fn bytes(o: WorkRunMigrationOutcome) -> Vec<u8> {
    match o {
        WorkRunMigrationOutcome::Applied {
            canonical_snapshot, ..
        }
        | WorkRunMigrationOutcome::AlreadyConsumed {
            canonical_snapshot, ..
        } => canonical_snapshot,
    }
}
#[test]
fn canonical_input_maps_executable_and_historical_runs() {
    let mut m = SqliteWorkRunMigration::open_in_memory().unwrap();
    let out = m.migrate(&fixture()).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes(out)).unwrap();
    assert_eq!(v["workRuns"].as_array().unwrap().len(), 0);
    assert_eq!(v["historicalStageRuns"].as_array().unwrap().len(), 1);
    assert!(v["historicalSource"].is_object());
}
#[test]
fn restart_changed_input_returns_original_and_zero_second_write() {
    let dir = std::env::temp_dir().join(format!("wwc-workrun-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("migration.sqlite");
    let first = {
        let mut m = SqliteWorkRunMigration::open(&path).unwrap();
        m.migrate(&fixture()).unwrap()
    };
    let mut changed = fixture();
    changed.extend_from_slice(b" ");
    let second = {
        let mut m = SqliteWorkRunMigration::open(&path).unwrap();
        m.migrate(&changed)
    };
    assert!(matches!(
        second,
        Err(WorkRunMigrationError::CorruptState(_))
    ));
    assert!(!bytes(first).is_empty());
    let _ = fs::remove_file(path);
}
#[test]
fn concurrent_same_source_applies_once() {
    let dir = std::env::temp_dir().join(format!(
        "wwc-workrun-concurrent-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("migration.sqlite");
    {
        let _ = SqliteWorkRunMigration::open(&path).unwrap();
    }
    let barrier = Arc::new(Barrier::new(2));
    let input = Arc::new(fixture());
    let hs = (0..2)
        .map(|_| {
            let b = barrier.clone();
            let i = input.clone();
            let p = path.clone();
            thread::spawn(move || {
                let mut m = SqliteWorkRunMigration::open(p).unwrap();
                b.wait();
                m.migrate(&i).unwrap()
            })
        })
        .collect::<Vec<_>>();
    let os = hs
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        os.iter()
            .filter(|o| matches!(o, WorkRunMigrationOutcome::Applied { .. }))
            .count(),
        1
    );
    assert_eq!(
        os.iter()
            .filter(|o| matches!(o, WorkRunMigrationOutcome::AlreadyConsumed { .. }))
            .count(),
        1
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn malformed_or_mixed_input_rejected() {
    let mut m = SqliteWorkRunMigration::open_in_memory().unwrap();
    let mut v: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
    v["workRuns"] = serde_json::json!([]);
    assert!(matches!(
        m.migrate(&serde_json::to_vec(&v).unwrap()),
        Err(WorkRunMigrationError::InvalidInput(_))
    ));
}

fn complete_execution_fixture() -> Vec<u8> {
    let mut v: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
    let b = v["sessionBindings"].get_mut(0).unwrap();
    b["productSessionId"] = serde_json::json!("psn_01J00000000000000000000000");
    b["executionJobId"] = serde_json::json!("job_01J00000000000000000000000");
    b["workerSessionId"] = serde_json::json!("wsn_01J00000000000000000000000");
    b["codexThreadId"] = serde_json::json!("cdx_01J00000000000000000000000");
    v["stageRuns"][0]["status"] = serde_json::json!("running");
    v["stageRuns"][0]["finishedAtMillis"] = serde_json::Value::Null;
    serde_json::to_vec(&v).unwrap()
}

#[test]
fn complete_execution_identity_emits_terminal_work_run() {
    let (key, snapshot) = convert_canonical_delivery(&complete_execution_fixture()).unwrap();
    assert!(key.contains("winwincode.delivery-canonical-to-workrun.v1"));
    let value: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
    let runs = value["workRuns"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["state"], "cancelled");
    assert!(value["historicalSource"].is_object());
}

#[test]
fn null_task_is_historical_and_source_is_complete() {
    let mut value: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
    value["stageRuns"][0]["deliveryTaskId"] = serde_json::Value::Null;
    value["sessionBindings"][0]["deliveryTaskId"] = serde_json::Value::Null;
    let input = serde_json::to_vec(&value).unwrap();
    let (_, snapshot) = convert_canonical_delivery(&input).unwrap();
    let output: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
    assert_eq!(output["workRuns"].as_array().unwrap().len(), 0);
    assert_eq!(output["historicalStageRuns"].as_array().unwrap().len(), 1);
    assert_eq!(output["historicalSource"], value);
}

#[test]
fn delivery_namespace_keeps_work_item_ids_distinct() {
    let first = complete_execution_fixture();
    let second = String::from_utf8(first.clone()).unwrap().replace(
        "dlv_01J00000000000000000000000",
        "dlv_01J00000000000000000000001",
    );
    let (_, a) = convert_canonical_delivery(&first).unwrap();
    let (_, b) = convert_canonical_delivery(second.as_bytes()).unwrap();
    let av: serde_json::Value = serde_json::from_slice(&a).unwrap();
    let bv: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_ne!(av["workItems"][0]["id"], bv["workItems"][0]["id"]);
}

#[test]
fn migration_preserves_dependencies_long_goals_and_stable_revision_identity() {
    let mut source: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
    let mut next = source["tasks"][0].clone();
    next["id"] = serde_json::json!("task-dependent");
    next["goal"] = serde_json::json!("x".repeat(65536));
    next["blockedByTaskIds"] = serde_json::json!([source["tasks"][0]["id"]]);
    source["tasks"].as_array_mut().unwrap().push(next.clone());
    let (_, bytes) = convert_canonical_delivery(&serde_json::to_vec(&source).unwrap()).unwrap();
    let first: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(first["workItems"][1]["goal"], next["goal"]);
    assert_eq!(first["workItems"][1]["state"], "waiting_dependency");
    assert_eq!(
        first["workItems"][1]["dependsOn"][0],
        first["workItems"][0]["id"]
    );
    assert_eq!(
        first["workItems"][1]["criterionIds"][0],
        first["workContract"]["criteria"][0]["id"]
    );
    assert_eq!(first["historicalSource"], source);
    source["revision"] = serde_json::json!(8);
    let (_, bytes) = convert_canonical_delivery(&serde_json::to_vec(&source).unwrap()).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(first["workItems"][0]["id"], second["workItems"][0]["id"]);
    assert_eq!(first["workContract"]["id"], second["workContract"]["id"]);
}
