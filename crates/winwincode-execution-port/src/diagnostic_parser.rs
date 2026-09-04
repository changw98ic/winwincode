// SPDX-License-Identifier: Apache-2.0

//! Versioned normalization of bounded validation diagnostics.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use serde_json::Value;
use sha2::{Digest, Sha256};
use winwincode_domain::{Sha256Digest, WorkspaceRevision};

use crate::generated::{
    DiagnosticBaseline, DiagnosticBaselineComparison, DiagnosticCategory, DiagnosticChangeStatus,
    DiagnosticComparisonEntry, DiagnosticParserVersion, DiagnosticSeverity, NormalizedDiagnostic,
};

/// Maximum bytes accepted from one command stream.
pub const MAX_DIAGNOSTIC_INPUT_BYTES: usize = 16_777_216;
/// Maximum diagnostics retained in one exact baseline.
pub const MAX_DIAGNOSTICS: usize = 4096;

const DIAGNOSTIC_ID_DOMAIN: &[u8] = b"winwincode.diagnostic-id.v1";
const DIAGNOSTIC_SET_DOMAIN: &[u8] = b"winwincode.diagnostic-set.v1";

/// Stable diagnostic parser failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticParseErrorCode {
    InputTooLarge,
    InvalidUtf8,
    InvalidPayload,
    TooManyDiagnostics,
    InvalidPath,
    InvalidBaseline,
}

/// Bounded parser failure that never includes raw tool output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticParseError {
    code: DiagnosticParseErrorCode,
    message: &'static str,
}

impl DiagnosticParseError {
    /// Returns the stable failure category.
    pub const fn code(&self) -> DiagnosticParseErrorCode {
        self.code
    }
}

impl fmt::Display for DiagnosticParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for DiagnosticParseError {}

/// One parsed command output before it is bound to a workspace revision.
#[derive(Clone, Debug, PartialEq)]
pub struct DiagnosticParseBatch {
    pub parser_version: DiagnosticParserVersion,
    pub diagnostics: Vec<NormalizedDiagnostic>,
}

/// Selects the canonical stream for every supported parser.
///
/// All v1 formats are required on stdout. Stderr is retained as a separate raw
/// Artifact but is never guessed to be structured diagnostic input.
pub const fn diagnostic_input<'output>(
    _version: &DiagnosticParserVersion,
    stdout: &'output [u8],
    _stderr: &'output [u8],
) -> &'output [u8] {
    stdout
}

/// Returns the canonical media type for the selected structured stdout Artifact.
pub const fn diagnostic_media_type(version: &DiagnosticParserVersion) -> &'static str {
    match version {
        DiagnosticParserVersion::EslintJsonV1 | DiagnosticParserVersion::PytestJsonV1 => {
            "application/json"
        }
        DiagnosticParserVersion::CargoJsonV1 | DiagnosticParserVersion::GoTestJsonV1 => {
            "application/x-ndjson"
        }
        DiagnosticParserVersion::TypescriptV1 => "text/plain; charset=utf-8",
        DiagnosticParserVersion::JunitXmlV1 => "application/xml",
    }
}

/// Normalizes one explicitly versioned structured command output.
///
/// # Errors
///
/// Rejects oversized, malformed, non-UTF-8, path-escaping, or overfull input.
pub fn parse_diagnostics(
    version: DiagnosticParserVersion,
    input: &[u8],
    workspace_root: &Path,
) -> Result<DiagnosticParseBatch, DiagnosticParseError> {
    if input.len() > MAX_DIAGNOSTIC_INPUT_BYTES {
        return Err(error(
            DiagnosticParseErrorCode::InputTooLarge,
            "diagnostic input exceeds the canonical byte limit",
        ));
    }
    let text = std::str::from_utf8(input).map_err(|_| {
        error(
            DiagnosticParseErrorCode::InvalidUtf8,
            "diagnostic input is not UTF-8",
        )
    })?;
    let drafts = match version {
        DiagnosticParserVersion::EslintJsonV1 => parse_eslint(text)?,
        DiagnosticParserVersion::TypescriptV1 => parse_typescript(text)?,
        DiagnosticParserVersion::CargoJsonV1 => parse_cargo(text)?,
        DiagnosticParserVersion::GoTestJsonV1 => parse_go_test(text)?,
        DiagnosticParserVersion::JunitXmlV1 => parse_junit(text)?,
        DiagnosticParserVersion::PytestJsonV1 => parse_pytest(text)?,
    };
    if drafts.len() > MAX_DIAGNOSTICS {
        return Err(too_many_diagnostics());
    }
    let mut diagnostics = drafts
        .into_iter()
        .map(|draft| normalize_diagnostic(&version, draft, workspace_root))
        .collect::<Result<Vec<_>, _>>()?;
    diagnostics.sort_by(|left, right| left.diagnostic_id.0.cmp(&right.diagnostic_id.0));
    diagnostics.dedup_by(|left, right| left.diagnostic_id == right.diagnostic_id);
    Ok(DiagnosticParseBatch {
        parser_version: version,
        diagnostics,
    })
}

/// Binds parsed batches to one exact workspace tree and canonical set digest.
///
/// # Errors
///
/// Rejects an empty or oversized batch set or too many diagnostics.
pub fn build_diagnostic_baseline(
    revision: WorkspaceRevision,
    batches: &[DiagnosticParseBatch],
) -> Result<DiagnosticBaseline, DiagnosticParseError> {
    if batches.is_empty() || batches.len() > 64 {
        return Err(invalid_baseline());
    }
    let mut parser_versions = batches
        .iter()
        .map(|batch| batch.parser_version.clone())
        .collect::<Vec<_>>();
    parser_versions.sort_by_key(parser_version_text);
    parser_versions.dedup();
    if parser_versions.len() > 6 {
        return Err(invalid_baseline());
    }
    let mut diagnostics = batches
        .iter()
        .flat_map(|batch| batch.diagnostics.iter().cloned())
        .collect::<Vec<_>>();
    if diagnostics.len() > MAX_DIAGNOSTICS {
        return Err(too_many_diagnostics());
    }
    if batches.iter().any(|batch| {
        batch.diagnostics.iter().any(|diagnostic| {
            diagnostic.parser_version != batch.parser_version
                || !valid_normalized_diagnostic(diagnostic)
        })
    }) {
        return Err(invalid_baseline());
    }
    diagnostics.sort_by(|left, right| left.diagnostic_id.0.cmp(&right.diagnostic_id.0));
    if diagnostics
        .windows(2)
        .any(|pair| pair[0].diagnostic_id == pair[1].diagnostic_id && pair[0] != pair[1])
    {
        return Err(invalid_baseline());
    }
    diagnostics.dedup_by(|left, right| left.diagnostic_id == right.diagnostic_id);
    let diagnostic_set_digest = diagnostic_set_digest(&parser_versions, &diagnostics);
    let baseline = DiagnosticBaseline {
        workspace_revision: revision,
        parser_versions,
        diagnostics,
        diagnostic_set_digest,
    };
    validate_diagnostic_baseline(&baseline)?;
    Ok(baseline)
}

/// Compares two validated baselines into deterministic new/resolved/unchanged entries.
///
/// # Errors
///
/// Rejects a baseline whose stored digest or parser inventory is not canonical.
pub fn compare_diagnostic_baselines(
    baseline: &DiagnosticBaseline,
    result: &DiagnosticBaseline,
) -> Result<DiagnosticBaselineComparison, DiagnosticParseError> {
    validate_diagnostic_baseline(baseline)?;
    validate_diagnostic_baseline(result)?;
    if baseline.parser_versions != result.parser_versions {
        return Err(invalid_baseline());
    }
    let before = baseline
        .diagnostics
        .iter()
        .map(|diagnostic| (diagnostic.diagnostic_id.0.as_str(), diagnostic))
        .collect::<BTreeMap<_, _>>();
    let after = result
        .diagnostics
        .iter()
        .map(|diagnostic| (diagnostic.diagnostic_id.0.as_str(), diagnostic))
        .collect::<BTreeMap<_, _>>();
    let ids = before
        .keys()
        .chain(after.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut entries = Vec::with_capacity(ids.len());
    let mut new_count = 0_i64;
    let mut resolved_count = 0_i64;
    let mut unchanged_count = 0_i64;
    for id in ids {
        let (status, diagnostic) = match (before.get(id), after.get(id)) {
            (None, Some(diagnostic)) => {
                new_count += 1;
                (DiagnosticChangeStatus::New, *diagnostic)
            }
            (Some(diagnostic), None) => {
                resolved_count += 1;
                (DiagnosticChangeStatus::Resolved, *diagnostic)
            }
            (Some(_), Some(diagnostic)) => {
                unchanged_count += 1;
                (DiagnosticChangeStatus::Unchanged, *diagnostic)
            }
            (None, None) => continue,
        };
        entries.push(DiagnosticComparisonEntry {
            status,
            diagnostic: diagnostic.clone(),
        });
    }
    Ok(DiagnosticBaselineComparison {
        base_revision: baseline.workspace_revision.clone(),
        result_revision: result.workspace_revision.clone(),
        baseline_digest: baseline.diagnostic_set_digest.clone(),
        result_digest: result.diagnostic_set_digest.clone(),
        entries,
        new_count,
        resolved_count,
        unchanged_count,
    })
}

/// Revalidates a persisted comparison against its two exact baselines.
///
/// # Errors
///
/// Rejects any revision, digest, entry, status, ordering, or count drift.
pub fn validate_diagnostic_baseline_comparison(
    comparison: &DiagnosticBaselineComparison,
    baseline: &DiagnosticBaseline,
    result: &DiagnosticBaseline,
) -> Result<(), DiagnosticParseError> {
    if &compare_diagnostic_baselines(baseline, result)? != comparison {
        return Err(invalid_baseline());
    }
    Ok(())
}

/// Returns the stable repair reason for the highest-priority new diagnostic.
pub fn dominant_diagnostic_repair_reason(
    comparison: &DiagnosticBaselineComparison,
) -> &'static str {
    let categories = comparison
        .entries
        .iter()
        .filter(|entry| entry.status == DiagnosticChangeStatus::New)
        .map(|entry| &entry.diagnostic.category)
        .collect::<Vec<_>>();
    for (category, reason) in [
        (
            DiagnosticCategory::Infrastructure,
            "diagnostic.infrastructure",
        ),
        (
            DiagnosticCategory::MissingModule,
            "diagnostic.missing_module",
        ),
        (
            DiagnosticCategory::MissingSymbol,
            "diagnostic.missing_symbol",
        ),
        (DiagnosticCategory::TypeMismatch, "diagnostic.type_mismatch"),
        (DiagnosticCategory::LintFailure, "diagnostic.lint_failure"),
        (DiagnosticCategory::TestFailure, "diagnostic.test_failure"),
        (DiagnosticCategory::Unclassified, "diagnostic.unclassified"),
    ] {
        if categories.contains(&&category) {
            return reason;
        }
    }
    "diagnostic.no_new_failures"
}

#[derive(Debug)]
struct DiagnosticDraft {
    path: String,
    code: String,
    severity: DiagnosticSeverity,
    line: Option<i64>,
    column: Option<i64>,
    message: String,
    category: Option<DiagnosticCategory>,
}

fn parse_eslint(text: &str) -> Result<Vec<DiagnosticDraft>, DiagnosticParseError> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let reports = serde_json::from_str::<Value>(text).map_err(|_| invalid_payload())?;
    let reports = reports.as_array().ok_or_else(invalid_payload)?;
    let mut drafts = Vec::new();
    for report in reports {
        let path = string_field(report, "filePath")?;
        let messages = report
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(invalid_payload)?;
        for message in messages {
            let raw_code = message
                .get("ruleId")
                .and_then(Value::as_str)
                .unwrap_or("eslint");
            let severity = match message.get("severity").and_then(Value::as_i64) {
                Some(2) => DiagnosticSeverity::Error,
                Some(1) => DiagnosticSeverity::Warning,
                Some(0) => DiagnosticSeverity::Info,
                _ => return Err(invalid_payload()),
            };
            drafts.push(DiagnosticDraft {
                path: path.to_owned(),
                code: format!("eslint:{raw_code}"),
                severity,
                line: optional_positive_i64(message.get("line"))?,
                column: optional_positive_i64(message.get("column"))?,
                message: string_field(message, "message")?.to_owned(),
                category: Some(DiagnosticCategory::LintFailure),
            });
        }
    }
    Ok(drafts)
}

fn parse_typescript(text: &str) -> Result<Vec<DiagnosticDraft>, DiagnosticParseError> {
    let mut drafts = Vec::new();
    for line in text.lines() {
        let Some(error_offset) = line.find(": error TS") else {
            continue;
        };
        let location = &line[..error_offset];
        let detail = &line[error_offset + 2..];
        let Some(code_end) = detail.find(':') else {
            return Err(invalid_payload());
        };
        let code = detail[..code_end]
            .strip_prefix("error ")
            .ok_or_else(invalid_payload)?;
        let (path, line_number, column) = parse_parenthesized_location(location)?;
        drafts.push(DiagnosticDraft {
            path,
            code: code.to_owned(),
            severity: DiagnosticSeverity::Error,
            line: Some(line_number),
            column: Some(column),
            message: detail[code_end + 1..].trim().to_owned(),
            category: None,
        });
    }
    Ok(drafts)
}

fn parse_cargo(text: &str) -> Result<Vec<DiagnosticDraft>, DiagnosticParseError> {
    let mut drafts = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        if !line.trim_start().starts_with('{') {
            continue;
        }
        let value = serde_json::from_str::<Value>(line).map_err(|_| invalid_payload())?;
        if value.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let message = value.get("message").ok_or_else(invalid_payload)?;
        let severity = severity_from_text(string_field(message, "level")?)?;
        if !matches!(
            severity,
            DiagnosticSeverity::Error | DiagnosticSeverity::Warning
        ) {
            continue;
        }
        let code = message
            .get("code")
            .and_then(|code| code.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("rustc");
        let span = message
            .get("spans")
            .and_then(Value::as_array)
            .and_then(|spans| {
                spans
                    .iter()
                    .find(|span| span.get("is_primary").and_then(Value::as_bool) == Some(true))
                    .or_else(|| spans.first())
            });
        drafts.push(DiagnosticDraft {
            path: span
                .and_then(|span| span.get("file_name"))
                .and_then(Value::as_str)
                .unwrap_or(".")
                .to_owned(),
            code: code.to_owned(),
            severity,
            line: optional_positive_i64(span.and_then(|span| span.get("line_start")))?,
            column: optional_positive_i64(span.and_then(|span| span.get("column_start")))?,
            message: string_field(message, "message")?.to_owned(),
            category: None,
        });
    }
    Ok(drafts)
}

fn parse_go_test(input: &str) -> Result<Vec<DiagnosticDraft>, DiagnosticParseError> {
    let mut drafts = Vec::new();
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        let value = serde_json::from_str::<Value>(line).map_err(|_| invalid_payload())?;
        if value.get("Action").and_then(Value::as_str) != Some("fail") {
            continue;
        }
        let package = value.get("Package").and_then(Value::as_str).unwrap_or(".");
        let test_name = value.get("Test").and_then(Value::as_str);
        drafts.push(DiagnosticDraft {
            path: ".".to_owned(),
            code: "go_test_failure".to_owned(),
            severity: DiagnosticSeverity::Error,
            line: None,
            column: None,
            message: test_name.map_or_else(
                || format!("Go package {package} failed"),
                |test| format!("Go test {test} failed in {package}"),
            ),
            category: Some(DiagnosticCategory::TestFailure),
        });
    }
    Ok(drafts)
}

fn parse_junit(text: &str) -> Result<Vec<DiagnosticDraft>, DiagnosticParseError> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut current_test = JunitTestCase::default();
    let mut drafts = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) if event.name().as_ref() == b"testcase" => {
                current_test = junit_test_case(&reader, &event)?;
            }
            Ok(Event::Empty(event)) if event.name().as_ref() == b"testcase" => {
                current_test = JunitTestCase::default();
            }
            Ok(Event::Start(event) | Event::Empty(event))
                if matches!(event.name().as_ref(), b"failure" | b"error") =>
            {
                let message = xml_attribute(&reader, &event, b"message")?
                    .unwrap_or_else(|| "JUnit test failed".to_owned());
                drafts.push(DiagnosticDraft {
                    path: current_test.path.clone().unwrap_or_else(|| ".".to_owned()),
                    code: if event.name().as_ref() == b"error" {
                        "junit_error"
                    } else {
                        "junit_failure"
                    }
                    .to_owned(),
                    severity: DiagnosticSeverity::Error,
                    line: current_test.line,
                    column: None,
                    message: if current_test.name.is_empty() {
                        message
                    } else {
                        format!("{}: {message}", current_test.name)
                    },
                    category: Some(DiagnosticCategory::TestFailure),
                });
            }
            Ok(Event::DocType(_)) | Err(_) => return Err(invalid_payload()),
            Ok(Event::Eof) => break,
            Ok(_) => {}
        }
    }
    Ok(drafts)
}

fn parse_pytest(text: &str) -> Result<Vec<DiagnosticDraft>, DiagnosticParseError> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let report = serde_json::from_str::<Value>(text).map_err(|_| invalid_payload())?;
    let tests = optional_array_field(&report, "tests")?;
    let collectors = optional_array_field(&report, "collectors")?;
    if tests.is_none() && collectors.is_none() {
        return Err(invalid_payload());
    }
    let mut drafts = Vec::new();
    for test in tests.into_iter().flatten() {
        let outcome = string_field(test, "outcome")?;
        if !matches!(outcome, "failed" | "error") {
            continue;
        }
        let node_id = string_field(test, "nodeid")?;
        let phase = ["call", "setup", "teardown"]
            .into_iter()
            .filter_map(|name| test.get(name))
            .find(|phase| {
                phase
                    .get("outcome")
                    .and_then(Value::as_str)
                    .is_some_and(|outcome| matches!(outcome, "failed" | "error"))
                    || phase.get("crash").is_some()
                    || phase.get("longrepr").is_some()
            })
            .unwrap_or(&Value::Null);
        drafts.push(pytest_failure_draft(
            test,
            phase,
            node_id,
            "pytest test failed",
        )?);
    }
    for collector in collectors.into_iter().flatten() {
        let outcome = string_field(collector, "outcome")?;
        if !matches!(outcome, "failed" | "error") {
            continue;
        }
        let node_id = string_field(collector, "nodeid")?;
        drafts.push(pytest_failure_draft(
            collector,
            collector,
            node_id,
            "pytest collection failed",
        )?);
    }
    Ok(drafts)
}

fn pytest_failure_draft(
    entry: &Value,
    detail: &Value,
    node_id: &str,
    fallback: &'static str,
) -> Result<DiagnosticDraft, DiagnosticParseError> {
    let crash = detail
        .get("crash")
        .or_else(|| entry.get("crash"))
        .unwrap_or(&Value::Null);
    let message = crash
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| detail.get("longrepr").and_then(Value::as_str))
        .or_else(|| entry.get("longrepr").and_then(Value::as_str))
        .unwrap_or(fallback);
    let code = if message.contains("ModuleNotFoundError") {
        "ModuleNotFoundError"
    } else if message.contains("NameError") {
        "NameError"
    } else {
        "pytest_failure"
    };
    Ok(DiagnosticDraft {
        path: crash
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_else(|| node_id.split("::").next().unwrap_or("."))
            .to_owned(),
        code: code.to_owned(),
        severity: DiagnosticSeverity::Error,
        line: optional_positive_i64(crash.get("lineno"))?,
        column: None,
        message: message.to_owned(),
        category: None,
    })
}

fn normalize_diagnostic(
    version: &DiagnosticParserVersion,
    draft: DiagnosticDraft,
    workspace_root: &Path,
) -> Result<NormalizedDiagnostic, DiagnosticParseError> {
    let path = normalize_path(&draft.path, workspace_root)?;
    let code = normalize_code(&draft.code);
    let category = draft
        .category
        .unwrap_or_else(|| classify(version, &code, &draft.message));
    let message_digest = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(draft.message.as_bytes())
    ));
    let display = bounded_display(&draft.message, &code);
    let diagnostic_id = derive_diagnostic_id(&DiagnosticIdentityFields {
        version,
        path: &path,
        code: &code,
        severity: &draft.severity,
        line: draft.line,
        column: draft.column,
        category: &category,
        message_digest: &message_digest,
    });
    Ok(NormalizedDiagnostic {
        diagnostic_id,
        parser_version: version.clone(),
        path,
        code,
        severity: draft.severity,
        line: draft.line,
        column: draft.column,
        category,
        message_digest,
        display,
    })
}

fn classify(version: &DiagnosticParserVersion, code: &str, message: &str) -> DiagnosticCategory {
    match (version, code) {
        (DiagnosticParserVersion::TypescriptV1, "TS2304" | "TS2305")
        | (DiagnosticParserVersion::CargoJsonV1, "E0425")
        | (DiagnosticParserVersion::PytestJsonV1, "NameError") => DiagnosticCategory::MissingSymbol,
        (DiagnosticParserVersion::TypescriptV1, "TS2307")
        | (DiagnosticParserVersion::CargoJsonV1, "E0432" | "E0433")
        | (DiagnosticParserVersion::PytestJsonV1, "ModuleNotFoundError") => {
            DiagnosticCategory::MissingModule
        }
        (DiagnosticParserVersion::TypescriptV1, "TS2322")
        | (DiagnosticParserVersion::CargoJsonV1, "E0308") => DiagnosticCategory::TypeMismatch,
        (DiagnosticParserVersion::EslintJsonV1, _) => DiagnosticCategory::LintFailure,
        (DiagnosticParserVersion::GoTestJsonV1 | DiagnosticParserVersion::JunitXmlV1, _) => {
            DiagnosticCategory::TestFailure
        }
        (DiagnosticParserVersion::PytestJsonV1, _) if message.contains("ModuleNotFoundError") => {
            DiagnosticCategory::MissingModule
        }
        (DiagnosticParserVersion::PytestJsonV1, _) if message.contains("NameError") => {
            DiagnosticCategory::MissingSymbol
        }
        (DiagnosticParserVersion::PytestJsonV1, _) => DiagnosticCategory::TestFailure,
        _ => DiagnosticCategory::Unclassified,
    }
}

struct DiagnosticIdentityFields<'field> {
    version: &'field DiagnosticParserVersion,
    path: &'field str,
    code: &'field str,
    severity: &'field DiagnosticSeverity,
    line: Option<i64>,
    column: Option<i64>,
    category: &'field DiagnosticCategory,
    message_digest: &'field Sha256Digest,
}

fn derive_diagnostic_id(fields: &DiagnosticIdentityFields<'_>) -> Sha256Digest {
    let mut digest = Sha256::new();
    digest.update(DIAGNOSTIC_ID_DOMAIN);
    for field in [
        parser_version_text(fields.version),
        fields.path,
        fields.code,
        severity_text(fields.severity),
        category_text(fields.category),
        &fields.message_digest.0,
    ] {
        digest_field(&mut digest, field.as_bytes());
    }
    digest_optional_i64(&mut digest, fields.line);
    digest_optional_i64(&mut digest, fields.column);
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

fn diagnostic_set_digest(
    versions: &[DiagnosticParserVersion],
    diagnostics: &[NormalizedDiagnostic],
) -> Sha256Digest {
    let mut digest = Sha256::new();
    digest.update(DIAGNOSTIC_SET_DOMAIN);
    digest.update(
        u64::try_from(versions.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for version in versions {
        digest_field(&mut digest, parser_version_text(version).as_bytes());
    }
    digest.update(
        u64::try_from(diagnostics.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for diagnostic in diagnostics {
        digest_field(&mut digest, diagnostic.diagnostic_id.0.as_bytes());
    }
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

/// Revalidates a persisted diagnostic baseline before replay or comparison.
///
/// # Errors
///
/// Rejects revision, ordering, bounds, identity, parser inventory, or digest drift.
pub fn validate_diagnostic_baseline(
    baseline: &DiagnosticBaseline,
) -> Result<(), DiagnosticParseError> {
    if baseline.parser_versions.is_empty()
        || baseline.parser_versions.len() > 6
        || baseline.diagnostics.len() > MAX_DIAGNOSTICS
        || !valid_workspace_revision(&baseline.workspace_revision)
    {
        return Err(invalid_baseline());
    }
    let mut versions = baseline.parser_versions.clone();
    versions.sort_by_key(parser_version_text);
    versions.dedup();
    let ids = baseline
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.diagnostic_id.0.as_str())
        .collect::<BTreeSet<_>>();
    let sorted_ids = ids.iter().copied().collect::<Vec<_>>();
    if versions != baseline.parser_versions
        || ids.len() != baseline.diagnostics.len()
        || sorted_ids
            != baseline
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.diagnostic_id.0.as_str())
                .collect::<Vec<_>>()
        || baseline.diagnostics.iter().any(|diagnostic| {
            !versions.contains(&diagnostic.parser_version)
                || !valid_normalized_diagnostic(diagnostic)
        })
        || !valid_sha256_digest(&baseline.diagnostic_set_digest)
        || diagnostic_set_digest(&versions, &baseline.diagnostics) != baseline.diagnostic_set_digest
    {
        return Err(invalid_baseline());
    }
    Ok(())
}

fn valid_normalized_diagnostic(diagnostic: &NormalizedDiagnostic) -> bool {
    if !normalize_path(&diagnostic.path, Path::new("")).is_ok_and(|path| path == diagnostic.path)
        || normalize_code(&diagnostic.code) != diagnostic.code
        || diagnostic.display.is_empty()
        || diagnostic.display.chars().count() > 500
        || diagnostic.display.contains(['\0', '\n', '\r'])
        || diagnostic
            .line
            .is_some_and(|line| !(1..=i64::from(i32::MAX)).contains(&line))
        || diagnostic
            .column
            .is_some_and(|column| !(1..=i64::from(i32::MAX)).contains(&column))
        || !valid_sha256_digest(&diagnostic.message_digest)
        || !valid_sha256_digest(&diagnostic.diagnostic_id)
    {
        return false;
    }
    derive_diagnostic_id(&DiagnosticIdentityFields {
        version: &diagnostic.parser_version,
        path: &diagnostic.path,
        code: &diagnostic.code,
        severity: &diagnostic.severity,
        line: diagnostic.line,
        column: diagnostic.column,
        category: &diagnostic.category,
        message_digest: &diagnostic.message_digest,
    }) == diagnostic.diagnostic_id
}

fn valid_workspace_revision(revision: &WorkspaceRevision) -> bool {
    let Some(value) = revision.0.strip_prefix("git-tree:") else {
        return false;
    };
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_sha256_digest(digest: &Sha256Digest) -> bool {
    digest.0.len() == 71
        && digest.0.strip_prefix("sha256:").is_some_and(|value| {
            value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

fn normalize_path(raw: &str, workspace_root: &Path) -> Result<String, DiagnosticParseError> {
    let raw = raw.replace('\\', "/");
    let root = workspace_root.to_string_lossy().replace('\\', "/");
    let relative = if raw == root {
        "."
    } else if let Some(relative) = raw.strip_prefix(&(root + "/")) {
        relative
    } else if raw.starts_with('/') || has_windows_drive(&raw) {
        return Err(invalid_path());
    } else {
        raw.as_str()
    };
    let mut parts = Vec::new();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." | ".git" | ".GIT" | ".Git" => return Err(invalid_path()),
            value if value.chars().any(char::is_control) => return Err(invalid_path()),
            value => parts.push(value),
        }
    }
    let normalized = if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    };
    if normalized.chars().count() > 4096 {
        return Err(invalid_path());
    }
    Ok(normalized)
}

fn has_windows_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn normalize_code(raw: &str) -> String {
    let mut result = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || "._:/@-".contains(character) {
                character
            } else {
                '_'
            }
        })
        .take(100)
        .collect::<String>();
    if result.is_empty() || !result.as_bytes()[0].is_ascii_alphanumeric() {
        result.insert_str(0, "diagnostic:");
        result.truncate(100);
    }
    result
}

fn bounded_display(message: &str, code: &str) -> String {
    let collapsed = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let source = if collapsed.is_empty() {
        code
    } else {
        &collapsed
    };
    source.chars().take(500).collect()
}

fn parser_version_text(version: &DiagnosticParserVersion) -> &'static str {
    match version {
        DiagnosticParserVersion::EslintJsonV1 => "eslint_json_v1",
        DiagnosticParserVersion::TypescriptV1 => "typescript_v1",
        DiagnosticParserVersion::CargoJsonV1 => "cargo_json_v1",
        DiagnosticParserVersion::GoTestJsonV1 => "go_test_json_v1",
        DiagnosticParserVersion::JunitXmlV1 => "junit_xml_v1",
        DiagnosticParserVersion::PytestJsonV1 => "pytest_json_v1",
    }
}

fn severity_text(severity: &DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => "error",
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Info => "info",
    }
}

fn category_text(category: &DiagnosticCategory) -> &'static str {
    match category {
        DiagnosticCategory::MissingSymbol => "missing_symbol",
        DiagnosticCategory::MissingModule => "missing_module",
        DiagnosticCategory::TypeMismatch => "type_mismatch",
        DiagnosticCategory::LintFailure => "lint_failure",
        DiagnosticCategory::TestFailure => "test_failure",
        DiagnosticCategory::Infrastructure => "infrastructure",
        DiagnosticCategory::Unclassified => "unclassified",
    }
}

fn digest_field(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

fn digest_optional_i64(digest: &mut Sha256, value: Option<i64>) {
    match value {
        None => digest.update([0]),
        Some(value) => {
            digest.update([1]);
            digest.update(value.to_be_bytes());
        }
    }
}

fn parse_parenthesized_location(value: &str) -> Result<(String, i64, i64), DiagnosticParseError> {
    let close = value.strip_suffix(')').ok_or_else(invalid_payload)?;
    let open = close.rfind('(').ok_or_else(invalid_payload)?;
    let mut location = close[open + 1..].split(',');
    let line = location
        .next()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(invalid_payload)?;
    let column = location
        .next()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(invalid_payload)?;
    if location.next().is_some() {
        return Err(invalid_payload());
    }
    Ok((close[..open].to_owned(), line, column))
}

fn severity_from_text(value: &str) -> Result<DiagnosticSeverity, DiagnosticParseError> {
    match value {
        "error" | "failure-note" => Ok(DiagnosticSeverity::Error),
        "warning" => Ok(DiagnosticSeverity::Warning),
        "note" | "help" => Ok(DiagnosticSeverity::Info),
        _ => Err(invalid_payload()),
    }
}

fn string_field<'value>(
    value: &'value Value,
    name: &str,
) -> Result<&'value str, DiagnosticParseError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(invalid_payload)
}

fn optional_array_field<'value>(
    value: &'value Value,
    name: &str,
) -> Result<Option<&'value [Value]>, DiagnosticParseError> {
    value
        .get(name)
        .map(|value| {
            value
                .as_array()
                .map(Vec::as_slice)
                .ok_or_else(invalid_payload)
        })
        .transpose()
}

fn optional_positive_i64(value: Option<&Value>) -> Result<Option<i64>, DiagnosticParseError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .filter(|value| *value > 0 && *value <= i64::from(i32::MAX))
            .map(Some)
            .ok_or_else(invalid_payload),
    }
}

#[derive(Default)]
struct JunitTestCase {
    name: String,
    path: Option<String>,
    line: Option<i64>,
}

fn junit_test_case(
    reader: &Reader<&[u8]>,
    event: &BytesStart<'_>,
) -> Result<JunitTestCase, DiagnosticParseError> {
    Ok(JunitTestCase {
        name: xml_attribute(reader, event, b"name")?.unwrap_or_default(),
        path: xml_attribute(reader, event, b"file")?,
        line: xml_attribute(reader, event, b"line")?
            .map(|line| line.parse::<i64>().map_err(|_| invalid_payload()))
            .transpose()?,
    })
}

fn xml_attribute(
    reader: &Reader<&[u8]>,
    event: &BytesStart<'_>,
    name: &[u8],
) -> Result<Option<String>, DiagnosticParseError> {
    for attribute in event.attributes() {
        let attribute = attribute.map_err(|_| invalid_payload())?;
        if attribute.key.as_ref() == name {
            return attribute
                .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                .map(|value| Some(value.into_owned()))
                .map_err(|_| invalid_payload());
        }
    }
    Ok(None)
}

const fn error(code: DiagnosticParseErrorCode, message: &'static str) -> DiagnosticParseError {
    DiagnosticParseError { code, message }
}

const fn invalid_payload() -> DiagnosticParseError {
    error(
        DiagnosticParseErrorCode::InvalidPayload,
        "diagnostic payload is malformed",
    )
}

const fn invalid_path() -> DiagnosticParseError {
    error(
        DiagnosticParseErrorCode::InvalidPath,
        "diagnostic path is not a portable workspace-relative path",
    )
}

const fn too_many_diagnostics() -> DiagnosticParseError {
    error(
        DiagnosticParseErrorCode::TooManyDiagnostics,
        "diagnostic count exceeds the canonical limit",
    )
}

const fn invalid_baseline() -> DiagnosticParseError {
    error(
        DiagnosticParseErrorCode::InvalidBaseline,
        "diagnostic baseline is not canonical",
    )
}
