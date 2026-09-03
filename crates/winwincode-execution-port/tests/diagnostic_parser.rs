// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use serde_json::json;
use winwincode_domain::WorkspaceRevision;
use winwincode_execution_port::diagnostic_parser::{
    DiagnosticParseBatch, DiagnosticParseErrorCode, build_diagnostic_baseline,
    compare_diagnostic_baselines, diagnostic_input, diagnostic_media_type,
    dominant_diagnostic_repair_reason, parse_diagnostics, validate_diagnostic_baseline,
    validate_diagnostic_baseline_comparison,
};
use winwincode_execution_port::generated::{
    DiagnosticCategory, DiagnosticChangeStatus, DiagnosticParserVersion, NormalizedDiagnostic,
};

fn root() -> &'static Path {
    Path::new("/workspace")
}

fn fixture(name: &str) -> &'static [u8] {
    match name {
        "eslint" => include_bytes!("fixtures/diagnostics/eslint.json"),
        "typescript" => include_bytes!("fixtures/diagnostics/typescript.txt"),
        "cargo" => include_bytes!("fixtures/diagnostics/cargo.jsonl"),
        "go" => include_bytes!("fixtures/diagnostics/go-test.jsonl"),
        "junit" => include_bytes!("fixtures/diagnostics/junit.xml"),
        "pytest" => include_bytes!("fixtures/diagnostics/pytest.json"),
        _ => panic!("unknown fixture"),
    }
}

fn revision(character: char) -> WorkspaceRevision {
    WorkspaceRevision(format!("git-tree:{}", character.to_string().repeat(40)))
}

#[test]
fn all_six_versioned_fixtures_normalize_to_closed_diagnostics() {
    let cases = [
        (
            DiagnosticParserVersion::EslintJsonV1,
            "eslint",
            1,
            DiagnosticCategory::LintFailure,
        ),
        (
            DiagnosticParserVersion::TypescriptV1,
            "typescript",
            4,
            DiagnosticCategory::MissingSymbol,
        ),
        (
            DiagnosticParserVersion::CargoJsonV1,
            "cargo",
            4,
            DiagnosticCategory::MissingSymbol,
        ),
        (
            DiagnosticParserVersion::GoTestJsonV1,
            "go",
            1,
            DiagnosticCategory::TestFailure,
        ),
        (
            DiagnosticParserVersion::JunitXmlV1,
            "junit",
            1,
            DiagnosticCategory::TestFailure,
        ),
        (
            DiagnosticParserVersion::PytestJsonV1,
            "pytest",
            2,
            DiagnosticCategory::MissingModule,
        ),
    ];
    for (version, fixture_name, count, first_category) in cases {
        let parsed = parse_diagnostics(version.clone(), fixture(fixture_name), root())
            .expect("canonical fixture");
        assert_eq!(parsed.parser_version, version);
        assert_eq!(parsed.diagnostics.len(), count);
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|item| item.category == first_category)
        );
        for diagnostic in parsed.diagnostics {
            assert!(diagnostic.diagnostic_id.0.starts_with("sha256:"));
            assert!(diagnostic.message_digest.0.starts_with("sha256:"));
            assert!(!diagnostic.path.starts_with('/'));
            assert!(diagnostic.display.chars().count() <= 500);
            assert!(!diagnostic.display.contains(['\n', '\r', '\0']));
        }
    }
}

#[test]
fn hard_repair_classification_covers_typescript_rust_and_python_codes() {
    let typescript = parse_diagnostics(
        DiagnosticParserVersion::TypescriptV1,
        fixture("typescript"),
        root(),
    )
    .expect("TypeScript diagnostics");
    let cargo = parse_diagnostics(
        DiagnosticParserVersion::CargoJsonV1,
        fixture("cargo"),
        root(),
    )
    .expect("Cargo diagnostics");
    let pytest = parse_diagnostics(
        DiagnosticParserVersion::PytestJsonV1,
        fixture("pytest"),
        root(),
    )
    .expect("pytest diagnostics");
    let categories = |batch: &DiagnosticParseBatch| {
        batch
            .diagnostics
            .iter()
            .map(|item| (item.code.clone(), item.category.clone()))
            .collect::<Vec<_>>()
    };
    assert!(
        categories(&typescript).contains(&("TS2304".to_owned(), DiagnosticCategory::MissingSymbol))
    );
    assert!(
        categories(&typescript).contains(&("TS2307".to_owned(), DiagnosticCategory::MissingModule))
    );
    assert!(
        categories(&typescript).contains(&("TS2305".to_owned(), DiagnosticCategory::MissingSymbol))
    );
    assert!(
        categories(&typescript).contains(&("TS2322".to_owned(), DiagnosticCategory::TypeMismatch))
    );
    assert!(categories(&cargo).contains(&("E0425".to_owned(), DiagnosticCategory::MissingSymbol)));
    assert!(categories(&cargo).contains(&("E0432".to_owned(), DiagnosticCategory::MissingModule)));
    assert!(categories(&cargo).contains(&("E0433".to_owned(), DiagnosticCategory::MissingModule)));
    assert!(categories(&cargo).contains(&("E0308".to_owned(), DiagnosticCategory::TypeMismatch)));
    assert!(categories(&pytest).contains(&(
        "ModuleNotFoundError".to_owned(),
        DiagnosticCategory::MissingModule
    )));
    assert!(
        categories(&pytest).contains(&("NameError".to_owned(), DiagnosticCategory::MissingSymbol))
    );
    assert_eq!(
        typescript
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "TS2304")
            .expect("TS2304")
            .diagnostic_id
            .0,
        "sha256:b088098b1bd62840434792f5a80e695fd23ebbb6b5c3320379c4d9bff44595bf"
    );

    let collection_failure = parse_diagnostics(
        DiagnosticParserVersion::PytestJsonV1,
        br#"{"collectors":[{"nodeid":"tests/test_collection.py","outcome":"failed","longrepr":"ModuleNotFoundError: No module named 'dependency'"}]}"#,
        root(),
    )
    .expect("pytest collection diagnostic");
    assert_eq!(
        categories(&collection_failure),
        vec![(
            "ModuleNotFoundError".to_owned(),
            DiagnosticCategory::MissingModule
        )]
    );

    let setup_failure = parse_diagnostics(
        DiagnosticParserVersion::PytestJsonV1,
        br#"{"tests":[{"nodeid":"tests/test_setup.py::test_case","outcome":"failed","setup":{"outcome":"failed","crash":{"path":"tests/test_setup.py","lineno":3,"message":"NameError: name 'fixture' is not defined"}}}]}"#,
        root(),
    )
    .expect("pytest setup diagnostic");
    assert_eq!(
        categories(&setup_failure),
        vec![("NameError".to_owned(), DiagnosticCategory::MissingSymbol)]
    );
}

#[test]
fn baseline_comparison_is_exact_sorted_and_revision_bound() {
    let baseline_batch = parse_diagnostics(
        DiagnosticParserVersion::TypescriptV1,
        fixture("typescript"),
        root(),
    )
    .expect("baseline diagnostics");
    let result_batch = parse_diagnostics(
        DiagnosticParserVersion::TypescriptV1,
        b"src/one.ts(1,2): error TS2304: Cannot find name 'missing'.\nsrc/new.ts(7,8): error TS2307: Cannot find module './new'.\n",
        root(),
    )
    .expect("result diagnostics");
    let baseline =
        build_diagnostic_baseline(revision('a'), &[baseline_batch]).expect("canonical baseline");
    let result =
        build_diagnostic_baseline(revision('b'), &[result_batch]).expect("canonical result");
    let comparison = compare_diagnostic_baselines(&baseline, &result).expect("exact comparison");
    assert_eq!(comparison.base_revision, revision('a'));
    assert_eq!(comparison.result_revision, revision('b'));
    assert_eq!(comparison.new_count, 1);
    assert_eq!(comparison.resolved_count, 3);
    assert_eq!(comparison.unchanged_count, 1);
    assert_eq!(comparison.entries.len(), 5);
    assert_eq!(
        comparison
            .entries
            .iter()
            .filter(|entry| entry.status == DiagnosticChangeStatus::New)
            .count(),
        1
    );
    assert_eq!(
        dominant_diagnostic_repair_reason(&comparison),
        "diagnostic.missing_module"
    );
    validate_diagnostic_baseline_comparison(&comparison, &baseline, &result)
        .expect("persisted comparison");
    let mut changed_count = comparison.clone();
    changed_count.new_count += 1;
    assert_eq!(
        validate_diagnostic_baseline_comparison(&changed_count, &baseline, &result)
            .expect_err("count drift")
            .code(),
        DiagnosticParseErrorCode::InvalidBaseline
    );
    assert_eq!(
        baseline.diagnostic_set_digest.0,
        "sha256:72fecce2c199da48daa5be8dc37a9be99da3e94ae312fabfdf350a488b9c8045"
    );
}

#[test]
fn stream_path_size_and_generated_shape_boundaries_fail_closed() {
    assert_eq!(
        diagnostic_input(&DiagnosticParserVersion::CargoJsonV1, b"stdout", b"stderr"),
        b"stdout"
    );
    assert_eq!(
        diagnostic_media_type(&DiagnosticParserVersion::CargoJsonV1),
        "application/x-ndjson"
    );
    assert_eq!(
        diagnostic_media_type(&DiagnosticParserVersion::TypescriptV1),
        "text/plain; charset=utf-8"
    );
    assert_eq!(
        diagnostic_media_type(&DiagnosticParserVersion::JunitXmlV1),
        "application/xml"
    );
    let outside = br#"[{"filePath":"/outside/secret.ts","messages":[{"ruleId":"x","severity":2,"message":"bad","line":1,"column":1}]}]"#;
    assert_eq!(
        parse_diagnostics(DiagnosticParserVersion::EslintJsonV1, outside, root())
            .expect_err("foreign absolute path")
            .code(),
        DiagnosticParseErrorCode::InvalidPath
    );
    let oversized = vec![b'x'; 16_777_217];
    assert_eq!(
        parse_diagnostics(DiagnosticParserVersion::TypescriptV1, &oversized, root())
            .expect_err("oversized input")
            .code(),
        DiagnosticParseErrorCode::InputTooLarge
    );
    let overfull = (1..=4097)
        .map(|line| format!("src/overfull.ts({line},1): error TS2304: missing-{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        parse_diagnostics(
            DiagnosticParserVersion::TypescriptV1,
            overfull.as_bytes(),
            root(),
        )
        .expect_err("overfull diagnostic set")
        .code(),
        DiagnosticParseErrorCode::TooManyDiagnostics
    );

    let parsed = parse_diagnostics(
        DiagnosticParserVersion::TypescriptV1,
        fixture("typescript"),
        root(),
    )
    .expect("diagnostic");
    let mut wire = serde_json::to_value(&parsed.diagnostics[0]).expect("wire");
    wire.as_object_mut()
        .expect("object")
        .insert("unknown".to_owned(), json!(true));
    assert!(serde_json::from_value::<NormalizedDiagnostic>(wire).is_err());
}

#[test]
fn malformed_json_xml_utf8_and_baseline_digest_are_rejected() {
    assert_eq!(
        build_diagnostic_baseline(revision('a'), &[])
            .expect_err("empty batch inventory")
            .code(),
        DiagnosticParseErrorCode::InvalidBaseline
    );
    assert_eq!(
        parse_diagnostics(DiagnosticParserVersion::EslintJsonV1, b"{}", root())
            .expect_err("wrong ESLint shape")
            .code(),
        DiagnosticParseErrorCode::InvalidPayload
    );
    assert_eq!(
        parse_diagnostics(
            DiagnosticParserVersion::JunitXmlV1,
            b"<!DOCTYPE x><testsuite/>",
            root()
        )
        .expect_err("DOCTYPE rejected")
        .code(),
        DiagnosticParseErrorCode::InvalidPayload
    );
    assert_eq!(
        parse_diagnostics(DiagnosticParserVersion::TypescriptV1, &[0xff], root())
            .expect_err("non UTF-8")
            .code(),
        DiagnosticParseErrorCode::InvalidUtf8
    );
    let batch = parse_diagnostics(
        DiagnosticParserVersion::TypescriptV1,
        fixture("typescript"),
        root(),
    )
    .expect("diagnostic");
    let mut baseline =
        build_diagnostic_baseline(revision('a'), std::slice::from_ref(&batch)).expect("baseline");
    baseline.diagnostic_set_digest.0 = format!("sha256:{}", "0".repeat(64));
    let result = build_diagnostic_baseline(revision('b'), &[batch]).expect("result");
    assert_eq!(
        compare_diagnostic_baselines(&baseline, &result)
            .expect_err("digest tampering")
            .code(),
        DiagnosticParseErrorCode::InvalidBaseline
    );
    let mut identity_tampered = result;
    identity_tampered.diagnostics[0].category = DiagnosticCategory::Infrastructure;
    assert_eq!(
        validate_diagnostic_baseline(&identity_tampered)
            .expect_err("diagnostic identity tampering")
            .code(),
        DiagnosticParseErrorCode::InvalidBaseline
    );
}
