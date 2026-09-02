// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "test-support")]

mod support;

use std::{collections::BTreeSet, env, ffi::OsString, fs, path::PathBuf};

use support::differential_runner::{DifferentialPlan, run_differential_plan};

const INPUT_ENV: &str = "WINWINCODE_DELIVERY_DIFFERENTIAL_INPUT";
const OUTPUT_ENV: &str = "WINWINCODE_DELIVERY_DIFFERENTIAL_OUTPUT";
const RESULT_SCHEMA: &str = "winwincode.delivery-strongflow-rust-differential-result.v1";

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the machine entry validates every closed output branch before writing the result"
)]
fn machine_entry_executes_node_authored_plan_when_supplied() {
    let Some((plan_path, output_path)) = env_paths() else {
        return;
    };
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
}

fn env_paths() -> Option<(PathBuf, PathBuf)> {
    paired_paths(env::var_os(INPUT_ENV), env::var_os(OUTPUT_ENV))
        .unwrap_or_else(|message| panic!("{message}"))
}

fn paired_paths(
    input: Option<OsString>,
    output: Option<OsString>,
) -> Result<Option<(PathBuf, PathBuf)>, String> {
    match (input, output) {
        (None, None) => Ok(None),
        (Some(input), Some(output)) => Ok(Some((PathBuf::from(input), PathBuf::from(output)))),
        (Some(_), None) => Err(format!("{OUTPUT_ENV} must accompany {INPUT_ENV}")),
        (None, Some(_)) => Err(format!("{INPUT_ENV} must accompany {OUTPUT_ENV}")),
    }
}

#[test]
fn machine_entry_skips_only_when_both_plan_paths_are_absent() {
    assert_eq!(paired_paths(None, None).expect("both absent"), None);
    assert!(paired_paths(Some("plan.json".into()), None).is_err());
    assert!(paired_paths(None, Some("result.json".into())).is_err());
    assert_eq!(
        paired_paths(Some("plan.json".into()), Some("result.json".into())).expect("paired paths"),
        Some((PathBuf::from("plan.json"), PathBuf::from("result.json")))
    );
}
