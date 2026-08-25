// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "test-support")]

mod support;

use std::{collections::BTreeSet, env, fs, path::PathBuf};

use serde_json::{Map, Value, json};
use support::differential_runner::{
    DifferentialPlan, local_fixture_terminal_outcome_statuses, run_differential_plan,
};

const INPUT_ENV: &str = "WINWINCODE_DELIVERY_DIFFERENTIAL_INPUT";
const OUTPUT_ENV: &str = "WINWINCODE_DELIVERY_DIFFERENTIAL_OUTPUT";
const PLAN_SCHEMA: &str = "winwincode.delivery-strongflow-differential-plan.v2";
const RESULT_SCHEMA: &str = "winwincode.delivery-strongflow-rust-differential-result.v1";

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the machine entry validates every closed output branch before writing the result"
)]
fn rust_runner_executes_every_frozen_transcript_and_writes_complete_actual_json() {
    let (plan_path, output_path, owned_root) = env_paths().unwrap_or_else(local_plan_paths);
    let plan_bytes = fs::read(&plan_path).expect("differential execution plan");
    let plan: DifferentialPlan =
        serde_json::from_slice(&plan_bytes).expect("strict differential execution plan");

    let result = run_differential_plan(&plan).expect("Rust differential runner");
    assert_eq!(result["schemaVersion"], RESULT_SCHEMA);
    assert_eq!(result["oracleSchemaVersion"], plan.oracle_schema_version());
    let scenarios = result["scenarios"].as_array().expect("result scenarios");
    assert_eq!(scenarios.len(), 10);
    assert_eq!(
        scenarios
            .iter()
            .map(|scenario| scenario["id"].as_str().expect("scenario id"))
            .collect::<Vec<_>>(),
        [
            "success-closed-loop",
            "request-id-replay",
            "revision-conflict",
            "corruption-recovery",
            "task-dag",
            "candidate-invalidation",
            "attention",
            "inconclusive",
            "infra-error",
            "rework",
        ]
    );

    for (actual, planned) in scenarios.iter().zip(plan.scenarios()) {
        let commands = actual["commands"].as_array().expect("commands");
        assert!(
            commands
                .iter()
                .all(|command| command.get("response").is_some())
        );
        assert!(commands.iter().all(|command| {
            command
                .as_object()
                .expect("command object")
                .keys()
                .map(String::as_str)
                .eq(["kind", "request", "response", "sourceCommandIndexes"])
                && matches!(
                    command["kind"].as_str(),
                    Some(
                        "control-plane.command"
                            | "control-plane.query"
                            | "execution-port.message"
                            | "fixture.command"
                    )
                )
        }));
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["kind"] == "execution-port.message")
                .count(),
            planned.execution_port_message_count(),
            "every migrated binding or terminal Worker fact must become one typed ExecutionPort message"
        );
        assert_eq!(
            commands
                .iter()
                .flat_map(|command| {
                    command["sourceCommandIndexes"]
                        .as_array()
                        .expect("source command indexes")
                        .iter()
                        .map(|value| {
                            value
                                .as_u64()
                                .and_then(|value| usize::try_from(value).ok())
                                .expect("source command index")
                        })
                })
                .collect::<BTreeSet<_>>(),
            (0..planned.command_count()).collect::<BTreeSet<_>>()
        );
        assert_eq!(
            actual["observation"]
                .as_object()
                .expect("complete observation")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["events", "projection", "snapshot", "store", "verdict"]
        );
        assert!(actual["observation"]["store"]["journal"]["records"].is_array());
        assert_eq!(
            actual["observation"]["store"]
                .as_object()
                .expect("durable store observation")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["journal", "outbox", "receipts", "state"]
        );
    }

    fs::write(
        &output_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&result).expect("result JSON")
        ),
    )
    .expect("write differential result");

    if let Some(root) = owned_root {
        fs::remove_dir_all(root).expect("remove local runner fixture");
    }
}

fn env_paths() -> Option<(PathBuf, PathBuf, Option<PathBuf>)> {
    let input = env::var_os(INPUT_ENV)?;
    let output = env::var_os(OUTPUT_ENV)
        .unwrap_or_else(|| panic!("{OUTPUT_ENV} must accompany {INPUT_ENV}"));
    Some((PathBuf::from(input), PathBuf::from(output), None))
}

fn local_plan_paths() -> (PathBuf, PathBuf, Option<PathBuf>) {
    let root = unique_root("local-entry");
    fs::create_dir_all(&root).expect("local differential root");
    let oracle: Value = serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/oracles/delivery-strongflow-typescript.v1.json"
    ))
    .expect("committed oracle");
    let scenarios = oracle["scenarios"]
        .as_array()
        .expect("oracle scenarios")
        .iter()
        .map(|scenario| {
            let commands = scenario["commands"]
                .as_array()
                .expect("oracle commands")
                .iter()
                .map(|command| {
                    let mut planned = Map::new();
                    planned.insert("kind".into(), command["kind"].clone());
                    if let Some(request) = command.get("request") {
                        planned.insert("request".into(), hydrate(request.clone(), &root));
                    }
                    if let Some(input) = command.get("input") {
                        planned.insert("input".into(), hydrate(input.clone(), &root));
                    }
                    Value::Object(planned)
                })
                .collect::<Vec<_>>();
            json!({
                "id": scenario["id"],
                "commands": commands,
                "terminalOutcomeStatusBySourceCommandIndex":
                    local_fixture_terminal_outcome_statuses(scenario)
                        .expect("closed terminal outcome plan facts"),
            })
        })
        .collect::<Vec<_>>();
    let plan = json!({
        "schemaVersion": PLAN_SCHEMA,
        "oracleSchemaVersion": oracle["schemaVersion"],
        "bindings": {
            "ORACLE_ROOT": root,
            "NODE_EXECUTABLE": "/usr/bin/node",
            "AUTH_PROOF": "rust-differential-fixture-proof",
            "fixtureRandomIdentities": {},
        },
        "scenarios": scenarios,
    });
    let input = root.join("plan.json");
    let output = root.join("result.json");
    fs::write(
        &input,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&plan).expect("plan JSON")
        ),
    )
    .expect("write local execution plan");
    (input, output, Some(root))
}

fn hydrate(value: Value, root: &std::path::Path) -> Value {
    match value {
        Value::String(value) => Value::String(
            value
                .replace("<ORACLE_ROOT>", &root.to_string_lossy())
                .replace("<NODE_EXECUTABLE>", "/usr/bin/node")
                .replace("<AUTH_PROOF>", "rust-differential-fixture-proof"),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| hydrate(value, root))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, hydrate(value, root)))
                .collect(),
        ),
        scalar => scalar,
    }
}

fn unique_root(label: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "winwincode-delivery-differential-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos()
    ))
}
