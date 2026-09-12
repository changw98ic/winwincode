// SPDX-License-Identifier: Apache-2.0

use winwincode_execution_port::generated::DebugContextSafetyScannerVersion;
use winwincode_worker::context_safety::{
    ContextSafetyError, MAX_SAFE_SNIPPET_CHARS, MAX_SAFE_SUMMARY_CHARS, ValidatedSafeText,
    WorkerContextSafetyScanner,
};

#[derive(Clone, Copy)]
enum TextKind {
    Summary,
    Snippet,
}

struct Case {
    expected_error: Option<ContextSafetyError>,
    input: String,
    kind: TextKind,
    name: &'static str,
}

impl Case {
    fn new(
        name: &'static str,
        kind: TextKind,
        input: impl Into<String>,
        expected_error: Option<ContextSafetyError>,
    ) -> Self {
        Self {
            expected_error,
            input: input.into(),
            kind,
            name,
        }
    }
}

fn boundary_cases() -> Vec<Case> {
    vec![
        Case::new(
            "ordinary-summary",
            TextKind::Summary,
            "One bounded hypothesis changed.",
            None,
        ),
        Case::new(
            "summary-exact-boundary",
            TextKind::Summary,
            "s".repeat(MAX_SAFE_SUMMARY_CHARS),
            None,
        ),
        Case::new(
            "summary-over-boundary",
            TextKind::Summary,
            "s".repeat(MAX_SAFE_SUMMARY_CHARS + 1),
            Some(ContextSafetyError::TooLong),
        ),
        Case::new(
            "snippet-exact-boundary",
            TextKind::Snippet,
            "x".repeat(MAX_SAFE_SNIPPET_CHARS),
            None,
        ),
        Case::new(
            "snippet-over-boundary",
            TextKind::Snippet,
            "x".repeat(MAX_SAFE_SNIPPET_CHARS + 1),
            Some(ContextSafetyError::TooLong),
        ),
        Case::new(
            "snippet-over-utf8-byte-boundary",
            TextKind::Snippet,
            "界".repeat((MAX_SAFE_SNIPPET_CHARS / 3) + 1),
            Some(ContextSafetyError::TooLong),
        ),
        Case::new(
            "empty",
            TextKind::Summary,
            "   ",
            Some(ContextSafetyError::Empty),
        ),
        Case::new(
            "multiline",
            TextKind::Snippet,
            "first line\nsecond line",
            Some(ContextSafetyError::NotSingleLine),
        ),
    ]
}

fn material_cases() -> Vec<Case> {
    vec![
        Case::new(
            "private-key",
            TextKind::Snippet,
            "-----BEGIN PRIVATE KEY-----",
            Some(ContextSafetyError::SensitiveMaterial),
        ),
        Case::new(
            "bearer-token",
            TextKind::Summary,
            "Authorization: Bearer abcdefghijklmnop",
            Some(ContextSafetyError::SensitiveMaterial),
        ),
        Case::new(
            "basic-token",
            TextKind::Summary,
            "Authorization: Basic YWJjZGVmZ2hpamts",
            Some(ContextSafetyError::SensitiveMaterial),
        ),
        Case::new(
            "jwt",
            TextKind::Snippet,
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.signature123",
            Some(ContextSafetyError::SensitiveMaterial),
        ),
        Case::new(
            "provider-token",
            TextKind::Summary,
            format!("github_pat_{}", "A".repeat(20)),
            Some(ContextSafetyError::SensitiveMaterial),
        ),
        Case::new(
            "url-userinfo",
            TextKind::Snippet,
            "https://user:password@example.invalid/path",
            Some(ContextSafetyError::SensitiveMaterial),
        ),
        Case::new(
            "sensitive-assignment",
            TextKind::Summary,
            "client_secret=abcdefgh",
            Some(ContextSafetyError::SensitiveMaterial),
        ),
        Case::new(
            "raw-log-marker",
            TextKind::Summary,
            "Attach the raw-log output.",
            Some(ContextSafetyError::RawContextMarker),
        ),
        Case::new(
            "explicit-redaction",
            TextKind::Summary,
            "token=[redacted]",
            None,
        ),
    ]
}

#[test]
fn safe_text_validation_is_table_driven() {
    let scanner = WorkerContextSafetyScanner;
    assert_eq!(
        scanner.profile().scanner_version,
        DebugContextSafetyScannerVersion::WorkspaceSecretScanV1
    );
    assert_eq!(
        scanner.profile().scanner_policy_digest.0,
        "sha256:38cf50679f8427e1265ced2c44500d3e5100dbd4a2346f165316340985f4eaf7"
    );

    let cases = boundary_cases().into_iter().chain(material_cases());
    for case in cases {
        let result = match case.kind {
            TextKind::Summary => ValidatedSafeText::try_summary(case.input.clone()),
            TextKind::Snippet => ValidatedSafeText::try_snippet(case.input.clone()),
        };
        assert_eq!(
            result.as_ref().err().copied(),
            case.expected_error,
            "{}",
            case.name
        );
        if let Ok(validated) = result {
            assert_eq!(validated.as_str(), case.input, "{}", case.name);
            assert_eq!(
                format!("{validated:?}"),
                "ValidatedSafeText(<validated>)",
                "{}",
                case.name
            );
            assert_eq!(validated.into_inner(), case.input, "{}", case.name);
        }
    }
}
