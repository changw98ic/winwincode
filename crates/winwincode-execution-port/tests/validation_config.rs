// SPDX-License-Identifier: Apache-2.0

use serde_json::json;
use winwincode_domain::WorkspaceRevision;
use winwincode_execution_port::{
    generated::{
        DiagnosticParserVersion, NormalizerReceipt, ValidationProfileName,
        ValidationProfileSelection, ValidationReceipt, ValidationSelectionSource,
    },
    validation_config::{
        ValidationConfigurationErrorCode, parse_validation_configuration,
        resolve_validation_profile, select_configured_profile, suggest_validation_profile,
        validate_normalizer_receipt_binding, validate_validation_receipt_binding,
    },
};

const CONFIGURATION: &str = include_str!("../../../.winwincode/validation.toml");

fn revision(value: char) -> WorkspaceRevision {
    WorkspaceRevision(format!("git-tree:{}", value.to_string().repeat(40)))
}

#[test]
fn repository_configuration_is_strict_and_selectable() {
    let parsed = parse_validation_configuration(CONFIGURATION).expect("configuration parses");
    assert_eq!(parsed.configuration().schema_version, 1);
    assert_eq!(parsed.configuration().profiles.len(), 4);
    assert_eq!(parsed.configuration().commands.len(), 6);
    assert_eq!(
        parsed
            .configuration()
            .commands
            .iter()
            .find(|command| command.id == "typescript-check")
            .expect("TypeScript command")
            .diagnostic_parser_version,
        Some(DiagnosticParserVersion::TypescriptV1)
    );
    assert!(
        parsed
            .configuration()
            .commands
            .iter()
            .find(|command| command.id == "rust-format")
            .expect("Writer command")
            .diagnostic_parser_version
            .is_none()
    );

    let selection =
        select_configured_profile(&parsed, "fast", &["crates/example/src/lib.rs".to_owned()])
            .expect("configured profile exists");
    assert_eq!(selection.profile, ValidationProfileName::Fast);
    assert_eq!(
        selection.source,
        ValidationSelectionSource::ExplicitConfiguration
    );
    assert!(selection.executable);
    assert_eq!(
        selection.configuration_digest,
        Some(parsed.digest().clone())
    );
    assert!(!selection.command_ids.is_empty());
}

#[test]
fn parser_rejects_unknown_version_network_shell_and_incomplete_language_set() {
    let cases = [
        CONFIGURATION.replacen("schemaVersion = 1", "schemaVersion = 2", 1),
        CONFIGURATION.replacen(
            "schemaVersion = 1",
            "schemaVersion = 1\nunknownField = true",
            1,
        ),
        CONFIGURATION.replacen("network = false", "network = true", 1),
        CONFIGURATION.replacen("phase = \"formatter\"", "phase = \"arbitrary_writer\"", 1),
        CONFIGURATION.replacen(
            "argv = [\"cargo\", \"fmt\", \"--all\"]",
            "argv = [\"sh\", \"-c\", \"cargo fmt --all\"]",
            1,
        ),
        CONFIGURATION.replace("language = \"python\"", "language = \"typescript\""),
        CONFIGURATION.replacen(
            "phase = \"formatter\"",
            "phase = \"formatter\"\ndiagnosticParserVersion = \"cargo_json_v1\"",
            1,
        ),
        CONFIGURATION.replacen(
            "diagnosticParserVersion = \"typescript_v1\"",
            "diagnosticParserVersion = \"cargo_json_v1\"",
            1,
        ),
    ];
    for case in cases {
        assert!(parse_validation_configuration(&case).is_err());
    }
}

#[test]
fn parser_rejects_duplicate_profiles_commands_environment_and_dangling_references() {
    let duplicate_profile = CONFIGURATION.replacen("name = \"fast\"", "name = \"changed\"", 1);
    let duplicate_command =
        CONFIGURATION.replacen("id = \"typescript-contracts\"", "id = \"rust-format\"", 1);
    let duplicate_environment = CONFIGURATION.replacen(
        "environment = []",
        "environment = [{ name = \"PATH\", value = \"/bin\" }, { name = \"PATH\", value = \"/usr/bin\" }]",
        1,
    );
    let dangling_reference = CONFIGURATION.replacen(
        "commandIds = [\"rust-format\", \"typescript-contracts\", \"python-syntax\"]",
        "commandIds = [\"missing-command\"]",
        1,
    );
    let unreferenced_command = CONFIGURATION.replacen(
        "[[profiles]]",
        "[[commands]]\nid = \"unused-check\"\nphase = \"validation\"\nlanguage = \"rust\"\nargv = [\"cargo\", \"check\"]\nworkingDirectory = \".\"\nallowedCompanionPaths = []\nenvironment = []\nnetwork = false\ntimeoutMillis = 300000\noutputLimitBytes = 1048576\n\n[[profiles]]",
        1,
    );
    for case in [
        duplicate_profile,
        duplicate_command,
        duplicate_environment,
        dangling_reference,
        unreferenced_command,
    ] {
        assert!(parse_validation_configuration(&case).is_err());
    }
}

#[test]
fn profiles_require_a_validation_command_after_the_writer_prefix() {
    let interleaved = CONFIGURATION.replacen(
        "commandIds = [\"rust-format\", \"typescript-contracts\", \"typescript-check\", \"python-syntax\"]",
        "commandIds = [\"typescript-contracts\", \"rust-format\", \"python-syntax\"]",
        1,
    );
    let writer_only = CONFIGURATION.replacen(
        "commandIds = [\"rust-format\", \"typescript-contracts\", \"python-syntax\"]",
        "commandIds = [\"rust-format\"]",
        1,
    );
    assert!(parse_validation_configuration(&interleaved).is_err());
    assert!(parse_validation_configuration(&writer_only).is_err());
}

#[test]
fn parser_rejects_argv_and_path_policy_boundaries() {
    let too_many_args = format!(
        "[{}]",
        std::iter::repeat_n("\"x\"", 257)
            .collect::<Vec<_>>()
            .join(",")
    );
    let aggregate_too_large = format!("[\"python3\", \"{}\"]", "x".repeat(65_536));
    let cases = [
        CONFIGURATION.replacen(
            "argv = [\"cargo\", \"fmt\", \"--all\"]",
            &format!("argv = {too_many_args}"),
            1,
        ),
        CONFIGURATION.replacen(
            "argv = [\"cargo\", \"fmt\", \"--all\"]",
            &format!("argv = {aggregate_too_large}"),
            1,
        ),
        CONFIGURATION.replacen(
            "workingDirectory = \".\"",
            "workingDirectory = \"../outside\"",
            1,
        ),
        CONFIGURATION.replacen(
            "workingDirectory = \".\"",
            "workingDirectory = \"C:/outside\"",
            1,
        ),
        CONFIGURATION.replacen("workingDirectory = \".\"", "workingDirectory = \"a//b\"", 1),
        CONFIGURATION.replacen(
            "workingDirectory = \".\"",
            "workingDirectory = \".git/hooks\"",
            1,
        ),
        CONFIGURATION.replacen("workingDirectory = \".\"", "workingDirectory = \"CON\"", 1),
        CONFIGURATION.replacen(
            "workingDirectory = \".\"",
            "workingDirectory = \"trailing.\"",
            1,
        ),
        CONFIGURATION.replacen(
            "workingDirectory = \".\"",
            "workingDirectory = \"wild*card\"",
            1,
        ),
        CONFIGURATION.replacen(
            "allowedCompanionPaths = []",
            "allowedCompanionPaths = [\"../outside\"]",
            1,
        ),
        CONFIGURATION.replacen(
            "phase = \"validation\"\nlanguage = \"typescript\"\nargv = [\"corepack\", \"pnpm\", \"contracts:check\"]\nworkingDirectory = \".\"\nallowedCompanionPaths = []",
            "phase = \"validation\"\nlanguage = \"typescript\"\nargv = [\"corepack\", \"pnpm\", \"contracts:check\"]\nworkingDirectory = \".\"\nallowedCompanionPaths = [\"generated.ts\"]",
            1,
        ),
    ];
    for case in cases {
        assert!(parse_validation_configuration(&case).is_err());
    }
}

#[test]
fn automatic_inference_is_advisory_only() {
    let lockfile = suggest_validation_profile(&["Cargo.lock".to_owned()]).expect("valid path");
    assert_eq!(lockfile.profile, ValidationProfileName::Affected);
    assert_eq!(
        lockfile.source,
        ValidationSelectionSource::AutomaticSuggestion
    );
    assert!(!lockfile.executable);
    assert!(lockfile.configuration_digest.is_none());
    assert!(lockfile.command_ids.is_empty());

    let mixed = suggest_validation_profile(&[
        "packages/contracts/src/index.ts".to_owned(),
        "crates/example/src/lib.rs".to_owned(),
    ])
    .expect("valid paths");
    assert_eq!(mixed.profile, ValidationProfileName::Affected);

    for paths in [
        vec!["../escape.rs".to_owned()],
        vec!["C:/escape.rs".to_owned()],
        vec!["a.rs".to_owned(), "a.rs".to_owned()],
    ] {
        assert!(suggest_validation_profile(&paths).is_err());
    }
}

#[test]
fn explicit_configuration_always_wins_over_automatic_inference() {
    let parsed = parse_validation_configuration(CONFIGURATION).expect("configuration");
    let explicit = resolve_validation_profile(Some(&parsed), "fast", &["Cargo.lock".to_owned()])
        .expect("explicit selection");
    assert_eq!(explicit.profile, ValidationProfileName::Fast);
    assert_eq!(
        explicit.source,
        ValidationSelectionSource::ExplicitConfiguration
    );
    assert!(explicit.executable);

    let suggestion = resolve_validation_profile(None, "fast", &["Cargo.lock".to_owned()])
        .expect("missing configuration suggestion");
    assert_eq!(suggestion.profile, ValidationProfileName::Affected);
    assert_eq!(
        suggestion.source,
        ValidationSelectionSource::AutomaticSuggestion
    );
    assert!(!suggestion.executable);
}

#[test]
fn generated_receipts_reject_unknown_fields_and_impossible_statuses() {
    let base = revision('a');
    let result = revision('b');
    let invalid = [
        json!({
            "status": "unchanged", "baseRevision": base.0,
            "resultRevision": result.0, "changedFileDigests": [format!("sha256:{}", "c".repeat(64))],
            "artifactRefs": []
        }),
        json!({
            "profile": "fast", "status": "passed", "baseRevision": base.0,
            "resultRevision": result.0, "checks": [], "durationMillis": 1,
            "artifactRefs": []
        }),
        json!({
            "profile": "fast", "status": "not_run", "baseRevision": base.0,
            "checks": [], "durationMillis": 0, "artifactRefs": [], "unknown": true
        }),
    ];
    assert!(serde_json::from_value::<NormalizerReceipt>(invalid[0].clone()).is_err());
    assert!(serde_json::from_value::<ValidationReceipt>(invalid[1].clone()).is_err());
    assert!(serde_json::from_value::<ValidationReceipt>(invalid[2].clone()).is_err());
    assert!(
        serde_json::from_value::<ValidationProfileSelection>(json!({
            "profile": "changed",
            "source": "automatic_suggestion",
            "executable": false,
            "changedPathsDigest": format!("sha256:{}", "d".repeat(64)),
            "commandIds": [],
            "reasonCode": "lockfile_changed"
        }))
        .is_err()
    );
}

#[test]
fn receipt_binding_requires_the_exact_workspace_revisions() {
    let base = revision('a');
    let result = revision('b');
    let digest = format!("sha256:{}", "c".repeat(64));
    let normalizer: NormalizerReceipt = serde_json::from_value(json!({
        "status": "normalized", "baseRevision": base.0,
        "resultRevision": result.0, "changedFileDigests": [digest], "artifactRefs": []
    }))
    .expect("canonical normalizer receipt");
    validate_normalizer_receipt_binding(&normalizer, &base, Some(&result))
        .expect("exact normalizer binding");
    assert_eq!(
        validate_normalizer_receipt_binding(&normalizer, &base, Some(&revision('d')))
            .expect_err("tree drift is rejected")
            .code(),
        ValidationConfigurationErrorCode::RevisionMismatch
    );

    let validation: ValidationReceipt = serde_json::from_value(json!({
        "profile": "fast", "status": "passed", "baseRevision": base.0,
        "resultRevision": result.0,
        "checks": [{"name":"typecheck", "status":"passed", "summary":"passed"}],
        "durationMillis": 10, "artifactRefs": []
    }))
    .expect("canonical validation receipt");
    validate_validation_receipt_binding(&validation, &base, Some(&result))
        .expect("exact validation binding");
    assert!(validate_validation_receipt_binding(&validation, &result, Some(&result)).is_err());

    let infrastructure: ValidationReceipt = serde_json::from_value(json!({
        "profile": "fast", "status": "infrastructure_error", "baseRevision": base.0,
        "resultRevision": result.0, "checks": [], "durationMillis": 10, "artifactRefs": []
    }))
    .expect("infrastructure outcome still binds the read-only tree");
    validate_validation_receipt_binding(&infrastructure, &base, Some(&result))
        .expect("exact infrastructure binding");
    assert!(validate_validation_receipt_binding(&infrastructure, &base, None).is_err());

    let cancelled: ValidationReceipt = serde_json::from_value(json!({
        "profile": "fast", "status": "cancelled", "baseRevision": base.0,
        "resultRevision": result.0, "checks": [], "durationMillis": 1,
        "artifactRefs": []
    }))
    .expect("cancelled outcome still binds the read-only tree");
    validate_validation_receipt_binding(&cancelled, &base, Some(&result))
        .expect("exact cancelled binding");
    assert!(validate_validation_receipt_binding(&cancelled, &base, None).is_err());
}
