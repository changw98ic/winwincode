// SPDX-License-Identifier: Apache-2.0

use serde::Deserialize;
use winwincode_execution_port::action_normalizer::{
    ActionIntent, ActionNormalization, ActionNormalizationErrorCode, ActionObject, ActionOperation,
    ActionPurpose, ActionRisk, ActionScope, FileAnalysis, FileOperation, FileRequest, McpRequest,
    NetworkRequest, ShellRequest, ToolRequest, normalize_action,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrozenFixture {
    schema_version: String,
    cases: Vec<FrozenCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrozenCase {
    name: String,
    intent: ActionIntent,
    request: ToolRequest,
    expected: ActionNormalization,
}

fn intent(
    object: ActionObject,
    operation: ActionOperation,
    scope: ActionScope,
    targets: &[&str],
    risk: ActionRisk,
) -> ActionIntent {
    ActionIntent {
        object,
        operation,
        intent: ActionPurpose::Implement,
        scope,
        targets: targets.iter().map(|target| (*target).to_owned()).collect(),
        requirement_refs: vec!["REQ-1".to_owned()],
        plan_refs: vec!["PLAN-1".to_owned()],
        expected_effect: "apply the requested change".to_owned(),
        scope_delta: None,
        rollback: Some("restore the prior candidate".to_owned()),
        executor_risk: risk,
    }
}

#[test]
fn frozen_fixture_covers_every_gateway_family_and_mismatch_explanations() {
    let fixture: FrozenFixture =
        serde_json::from_str(include_str!("fixtures/action-normalization.v1.json"))
            .expect("fixture must be valid JSON");
    assert_eq!(
        fixture.schema_version,
        winwincode_execution_port::action_normalizer::ACTION_NORMALIZATION_SCHEMA_VERSION
    );
    assert_eq!(fixture.cases.len(), 8);

    for case in fixture.cases {
        let actual = normalize_action(&case.intent, &case.request)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        assert_eq!(actual, case.expected, "{}", case.name);
        assert!(
            actual
                .comparison
                .mismatches
                .iter()
                .all(|mismatch| !mismatch.explanation.is_empty()),
            "{}",
            case.name
        );
    }
}

#[test]
fn the_same_input_has_byte_stable_json_output() {
    let intent = intent(
        ActionObject::ExternalResource,
        ActionOperation::Modify,
        ActionScope::External,
        &["mcp://fixture.server/update_record"],
        ActionRisk::High,
    );
    let first_request = ToolRequest::Mcp(McpRequest {
        server: "Fixture.Server".to_owned(),
        tool: "Update_Record".to_owned(),
        arguments: serde_json::json!({"z": 1, "a": {"y": 2, "b": 3}}),
    });
    let second_request = ToolRequest::Mcp(McpRequest {
        server: "Fixture.Server".to_owned(),
        tool: "Update_Record".to_owned(),
        arguments: serde_json::json!({"a": {"b": 3, "y": 2}, "z": 1}),
    });

    let first = serde_json::to_vec(&normalize_action(&intent, &first_request).expect("normalize"))
        .expect("encode");
    let second =
        serde_json::to_vec(&normalize_action(&intent, &second_request).expect("normalize"))
            .expect("encode");
    assert_eq!(first, second);
}

#[test]
fn paths_are_lexical_and_cannot_escape_the_workspace() {
    let intent = intent(
        ActionObject::ProductionCode,
        ActionOperation::Modify,
        ActionScope::Local,
        &["crates/kernel/src/lib.rs"],
        ActionRisk::Medium,
    );
    let normalized = normalize_action(
        &intent,
        &ToolRequest::File(FileRequest {
            operation: FileOperation::Write,
            paths: vec!["crates/kernel/src/./model/../lib.rs".to_owned()],
            analysis: FileAnalysis::default(),
        }),
    )
    .expect("lexical path");
    assert_eq!(normalized.observed.targets, ["crates/kernel/src/lib.rs"]);

    let error = normalize_action(
        &intent,
        &ToolRequest::File(FileRequest {
            operation: FileOperation::Write,
            paths: vec!["../outside".to_owned()],
            analysis: FileAnalysis::default(),
        }),
    )
    .expect_err("parent escape must fail");
    assert_eq!(error.code, ActionNormalizationErrorCode::InvalidPath);
}

#[test]
fn network_targets_drop_query_fragment_and_reject_credentials() {
    let intent = intent(
        ActionObject::ExternalResource,
        ActionOperation::Execute,
        ActionScope::External,
        &["GET https://api.example.test/v1/items"],
        ActionRisk::Low,
    );
    let normalized = normalize_action(
        &intent,
        &ToolRequest::Network(NetworkRequest {
            method: "get".to_owned(),
            url: "HTTPS://API.EXAMPLE.TEST:443/v1/items?TOKEN=secret#ignored".to_owned(),
        }),
    )
    .expect("normalize URL");
    assert_eq!(
        normalized.observed.targets,
        ["GET https://api.example.test/v1/items"]
    );

    let error = normalize_action(
        &intent,
        &ToolRequest::Network(NetworkRequest {
            method: "GET".to_owned(),
            url: "https://TOKEN@api.example.test/v1/items".to_owned(),
        }),
    )
    .expect_err("credential-bearing URL must fail");
    assert_eq!(error.code, ActionNormalizationErrorCode::InvalidUrl);
    assert!(!error.to_string().contains("TOKEN"));
}

#[test]
fn argv_shell_requests_are_classified_without_execution() {
    let normalized = normalize_action(
        &intent(
            ActionObject::Test,
            ActionOperation::Execute,
            ActionScope::Repository,
            &["cwd:.", "argv:[\"cargo\",\"test\"]"],
            ActionRisk::Low,
        ),
        &ToolRequest::Shell(ShellRequest {
            program: "cargo".to_owned(),
            args: vec!["test".to_owned()],
            working_directory: ".".to_owned(),
        }),
    )
    .expect("normalize shell");
    assert!(normalized.comparison.matches);
    assert_eq!(normalized.observed.objects, [ActionObject::Test]);
}

#[test]
fn destructive_argv_is_delete_and_high_risk() {
    let normalized = normalize_action(
        &intent(
            ActionObject::ProductionCode,
            ActionOperation::Delete,
            ActionScope::Repository,
            &["cwd:.", "argv:[\"rm\",\"-rf\",\"target\"]"],
            ActionRisk::High,
        ),
        &ToolRequest::Shell(ShellRequest {
            program: "rm".to_owned(),
            args: vec!["-rf".to_owned(), "target".to_owned()],
            working_directory: ".".to_owned(),
        }),
    )
    .expect("normalize destructive argv");
    assert!(normalized.comparison.matches);
    assert_eq!(normalized.observed.operation, ActionOperation::Delete);
    assert_eq!(normalized.observed.minimum_risk, ActionRisk::High);
}

#[test]
fn the_normalizer_has_no_runtime_side_effect_imports() {
    let source = include_str!("../src/action_normalizer.rs");
    for forbidden in [
        "std::fs",
        "std::process",
        "std::net",
        "tokio::process",
        "tokio::net",
    ] {
        assert!(
            !source.contains(forbidden),
            "normalizer must not import {forbidden}"
        );
    }
}
