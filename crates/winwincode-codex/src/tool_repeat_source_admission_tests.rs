// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::path::PathBuf;

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    store: AdapterStore,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("wwc-smoke-repeat-{}", uuid::Uuid::now_v7()));
        let source = root.join("checkout");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("main.py"), "print(0)").unwrap();
        let store = AdapterStore::open(&root).unwrap();
        store
            .save_run(
                "run",
                &serde_json::json!({
                    "workspace":source,
                    "job":{"workInput":{"workContract":{"scope":["main.py", "*.py"]}}},
                }),
            )
            .unwrap();
        store.enable_tool_repeat_guard("run").unwrap();
        Self {
            root,
            source,
            store,
        }
    }

    fn call(&self, ordinal: usize) -> bool {
        self.store
            .admit_tool_output("run", &format!("model-{ordinal}"), &smoke(ordinal))
            .unwrap()
    }

    fn change_source(&self, ordinal: usize) {
        std::fs::write(self.source.join("main.py"), format!("print({ordinal})")).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn smoke(ordinal: usize) -> String {
    serde_json::json!({"type":"output_item_done", "item":{
        "type":"function_call", "call_id":format!("smoke-{ordinal}"),
        "name":"public_smoke", "namespace":"mcp__benchmark_public_smoke",
        "arguments":"{}",
    }})
    .to_string()
}

#[test]
fn different_source_with_empty_arguments_is_not_a_repeated_request() {
    let fixture = Fixture::new();
    for ordinal in 0..6 {
        fixture.change_source(ordinal);
        assert!(fixture.call(ordinal));
    }
    assert!(!fixture.store.tool_repeat_stopped("run").unwrap());
    let connection = Connection::open(fixture.store.path()).unwrap();
    let (requests, comparisons): (i64, i64) = connection
        .query_row(
            "SELECT COUNT(DISTINCT request_digest), COUNT(DISTINCT comparison_digest)
         FROM tool_repeat_admission WHERE run_key='run'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((requests, comparisons), (1, 6));
}

#[test]
fn same_source_sixth_call_is_blocked_even_when_logs_and_caches_change() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.source.join("__pycache__")).unwrap();
    for ordinal in 0..6 {
        std::fs::write(fixture.source.join("run.log"), format!("attempt {ordinal}")).unwrap();
        std::fs::write(
            fixture.source.join("__pycache__/generated.py"),
            ordinal.to_string(),
        )
        .unwrap();
        assert_eq!(fixture.call(ordinal), ordinal < 5);
    }
    assert!(fixture.store.tool_repeat_stopped("run").unwrap());
}

#[test]
fn source_change_after_five_calls_is_admitted_but_restoring_source_retains_its_count() {
    let fixture = Fixture::new();
    for ordinal in 0..5 {
        assert!(fixture.call(ordinal));
    }
    fixture.change_source(1);
    assert!(fixture.call(5));
    fixture.change_source(0);
    assert!(!fixture.call(6));
}

#[test]
fn replay_uses_original_request_without_reading_changed_or_missing_source() {
    let fixture = Fixture::new();
    assert!(fixture.call(0));
    fixture.change_source(1);
    let reopened = AdapterStore::open(&fixture.root).unwrap();
    assert!(
        reopened
            .admit_tool_output("run", "model-0", &smoke(0))
            .unwrap()
    );
    std::fs::remove_dir_all(&fixture.source).unwrap();
    assert!(
        reopened
            .admit_tool_output("run", "model-0", &smoke(0))
            .unwrap()
    );
    let mut changed: Value = serde_json::from_str(&smoke(0)).unwrap();
    changed["item"]["arguments"] = Value::String("{\"command\":\"id\"}".into());
    assert!(matches!(
        reopened.admit_tool_output("run", "model-0", &changed.to_string()),
        Err(AdapterStoreError::Conflict),
    ));
    let connection = Connection::open(reopened.path()).unwrap();
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM tool_repeat_admission WHERE run_key='run'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn legacy_admissions_keep_replay_and_stop_facts_without_fabricating_source() {
    let fixture = Fixture::new();
    for ordinal in 0..5 {
        assert!(fixture.call(ordinal));
    }
    let connection = Connection::open(fixture.store.path()).unwrap();
    connection
        .execute_batch(
            "DROP INDEX tool_repeat_comparison_idx;
         ALTER TABLE tool_repeat_admission DROP COLUMN comparison_digest;
         INSERT INTO tool_repeat_run(run_key, stopped) VALUES ('stopped', 1);",
        )
        .unwrap();
    drop(connection);
    let reopened = AdapterStore::open(&fixture.root).unwrap();
    assert!(
        reopened
            .admit_tool_output("run", "model-0", &smoke(0))
            .unwrap()
    );
    // Those old calls recorded no source identity and cannot be attributed to
    // the current checkout. The first source-aware call gets its own count.
    assert!(
        reopened
            .admit_tool_output("run", "model-5", &smoke(5))
            .unwrap()
    );
    assert!(reopened.tool_repeat_stopped("stopped").unwrap());
    assert!(
        !reopened
            .admit_tool_output("stopped", "model", &smoke(6))
            .unwrap()
    );
    let connection = Connection::open(reopened.path()).unwrap();
    let unknown: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM tool_repeat_admission WHERE comparison_digest IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unknown, 5);
}

#[test]
fn only_exact_benchmark_smoke_names_use_source_context() {
    let session = "01J00000000000000000000000";
    for server in [
        "benchmark_public_smoke".to_owned(),
        format!("benchmark_public_smoke_psn_{session}"),
    ] {
        assert!(is_public_smoke("public_smoke", &server));
        assert!(is_public_smoke("public_smoke", &format!("mcp__{server}")));
        assert!(is_public_smoke(
            &format!("mcp__{server}__public_smoke"),
            "functions"
        ));
    }
    for server in ["functions", "other", "benchmark_public_smoke_psn_invalid"] {
        assert!(!is_public_smoke("public_smoke", server));
    }
    let mut invalid: Value = serde_json::from_str(&smoke(0)).unwrap();
    invalid["item"]["arguments"] = Value::String("{\"sourceDirectory\":\"/other\"}".into());
    assert!(
        !tool_identity(&invalid.to_string())
            .unwrap()
            .unwrap()
            .public_smoke
    );
}
