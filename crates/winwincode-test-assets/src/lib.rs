// SPDX-License-Identifier: Apache-2.0

//! Deterministic, read-only analysis of candidate changes to test assets.
//!
//! The analyzer compares complete baseline and candidate file contents. It
//! never executes a test, changes a file, or decides whether a delivery passes.
//! Every finding carries the repository-relative path and the SHA-256 digest of
//! the content that produced it so a gate or verifier can bind the result to an
//! exact candidate.

pub mod manifest;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// One repository file at the baseline and candidate revisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChangedFile<'a> {
    pub path: &'a str,
    pub baseline: Option<&'a str>,
    pub candidate: Option<&'a str>,
}

/// The deterministic class assigned to a suspicious test change.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    TestDeleted,
    TestDisabled,
    AssertionWeakened,
    SnapshotBulkUpdate,
    TestDiscoveryChanged,
    CoverageThresholdLowered,
    FixtureOrMockMasking,
    EnvironmentBypass,
    ReviewRequired,
}

/// Whether the finding is deterministic enough to block or requires review.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingDisposition {
    Block,
    Review,
}

/// A line-level explanation tied to immutable file content.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindingEvidence {
    pub side: EvidenceSide,
    pub line: usize,
    pub excerpt: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSide {
    Baseline,
    Candidate,
}

/// A structured result. Raw file contents are deliberately excluded.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestManipulationFinding {
    pub kind: FindingKind,
    pub disposition: FindingDisposition,
    pub path: String,
    pub baseline_digest: Option<String>,
    pub candidate_digest: Option<String>,
    pub summary: String,
    pub evidence: Vec<FindingEvidence>,
}

/// Analyze all supplied changes and return a stable, deduplicated result.
#[must_use]
pub fn analyze_test_changes(files: &[ChangedFile<'_>]) -> Vec<TestManipulationFinding> {
    let mut findings = Vec::new();

    for file in files {
        analyze_file(file, &mut findings);
    }

    findings.sort();
    findings.dedup();
    findings
}

fn analyze_file(file: &ChangedFile<'_>, findings: &mut Vec<TestManipulationFinding>) {
    let test_asset = is_test_asset(file.path);

    if test_asset && file.baseline.is_some() && file.candidate.is_none() {
        findings.push(finding(
            file,
            FindingKind::TestDeleted,
            FindingDisposition::Block,
            "a baseline test asset was deleted",
            Vec::new(),
        ));
        return;
    }

    let (Some(baseline), Some(candidate)) = (file.baseline, file.candidate) else {
        return;
    };
    if baseline == candidate {
        return;
    }

    let baseline_lines = indexed_lines(baseline);
    let candidate_lines = indexed_lines(candidate);
    let added = added_lines(&baseline_lines, &candidate_lines);
    let removed = added_lines(&candidate_lines, &baseline_lines);

    analyze_disabled_test(file, &added, findings);
    analyze_assertions(file, test_asset, &added, &removed, findings);
    analyze_snapshot(file, &added, &removed, findings);
    analyze_discovery_config(file, &added, &removed, findings);
    analyze_coverage(file, baseline, candidate, findings);
    analyze_fixture_and_environment(file, &added, findings);
    analyze_unclassified_removal(file, test_asset, &removed, findings);
}

fn analyze_disabled_test(
    file: &ChangedFile<'_>,
    added: &[(usize, &str)],
    findings: &mut Vec<TestManipulationFinding>,
) {
    if let Some(evidence) = first_matching(added, is_disable_marker) {
        findings.push(finding(
            file,
            FindingKind::TestDisabled,
            FindingDisposition::Block,
            "a new test skip, ignore, todo, or disabled marker was introduced",
            vec![candidate_evidence(evidence)],
        ));
    }
}

fn analyze_assertions(
    file: &ChangedFile<'_>,
    test_asset: bool,
    added: &[(usize, &str)],
    removed: &[(usize, &str)],
    findings: &mut Vec<TestManipulationFinding>,
) {
    if test_asset {
        let removed_assertions = matching_lines(removed, is_assertion_line);
        let added_assertions = matching_lines(added, is_assertion_line);
        if !removed_assertions.is_empty() && added_assertions.len() < removed_assertions.len() {
            findings.push(finding(
                file,
                FindingKind::AssertionWeakened,
                FindingDisposition::Block,
                "candidate removes more assertion lines than it adds",
                removed_assertions
                    .into_iter()
                    .take(3)
                    .map(baseline_evidence)
                    .collect(),
            ));
        }
    }
}

fn analyze_snapshot(
    file: &ChangedFile<'_>,
    added: &[(usize, &str)],
    removed: &[(usize, &str)],
    findings: &mut Vec<TestManipulationFinding>,
) {
    if is_snapshot_path(file.path) {
        let changed_lines = added.len() + removed.len();
        if changed_lines >= 20 {
            findings.push(finding(
                file,
                FindingKind::SnapshotBulkUpdate,
                FindingDisposition::Review,
                "snapshot changed by at least twenty non-empty lines",
                added
                    .iter()
                    .take(3)
                    .copied()
                    .map(candidate_evidence)
                    .collect(),
            ));
        }
    }
}

fn analyze_discovery_config(
    file: &ChangedFile<'_>,
    added: &[(usize, &str)],
    removed: &[(usize, &str)],
    findings: &mut Vec<TestManipulationFinding>,
) {
    if is_test_discovery_config(file.path) {
        findings.push(finding(
            file,
            FindingKind::TestDiscoveryChanged,
            FindingDisposition::Review,
            "test runner or discovery configuration changed",
            added
                .iter()
                .chain(removed.iter())
                .take(3)
                .map(|line| candidate_or_baseline_evidence(line, added))
                .collect(),
        ));
    }
}

fn analyze_coverage(
    file: &ChangedFile<'_>,
    baseline: &str,
    candidate: &str,
    findings: &mut Vec<TestManipulationFinding>,
) {
    let baseline_thresholds = coverage_thresholds(baseline);
    let candidate_thresholds = coverage_thresholds(candidate);
    for (key, baseline_value) in baseline_thresholds {
        if let Some(candidate_value) = candidate_thresholds.get(&key)
            && *candidate_value < baseline_value
        {
            findings.push(finding(
                file,
                FindingKind::CoverageThresholdLowered,
                FindingDisposition::Block,
                &format!(
                    "coverage threshold {key} decreased from {baseline_value} to {candidate_value}"
                ),
                Vec::new(),
            ));
        }
    }
}

fn analyze_fixture_and_environment(
    file: &ChangedFile<'_>,
    added: &[(usize, &str)],
    findings: &mut Vec<TestManipulationFinding>,
) {
    if let Some(evidence) = first_matching(added, is_fixture_or_mock_masking) {
        findings.push(finding(
            file,
            FindingKind::FixtureOrMockMasking,
            FindingDisposition::Review,
            "a new fixture or mock can force a successful result",
            vec![candidate_evidence(evidence)],
        ));
    }

    if let Some(evidence) = first_matching(added, is_environment_bypass) {
        findings.push(finding(
            file,
            FindingKind::EnvironmentBypass,
            FindingDisposition::Review,
            "a new environment-dependent test bypass was introduced",
            vec![candidate_evidence(evidence)],
        ));
    }
}

fn analyze_unclassified_removal(
    file: &ChangedFile<'_>,
    test_asset: bool,
    removed: &[(usize, &str)],
    findings: &mut Vec<TestManipulationFinding>,
) {
    let has_finding_for_path = findings.iter().any(|finding| finding.path == file.path);
    if test_asset && !removed.is_empty() && !has_finding_for_path {
        findings.push(finding(
            file,
            FindingKind::ReviewRequired,
            FindingDisposition::Review,
            "test behavior was removed but no deterministic manipulation rule classified it",
            removed
                .iter()
                .copied()
                .take(3)
                .map(baseline_evidence)
                .collect(),
        ));
    }
}

fn finding(
    file: &ChangedFile<'_>,
    kind: FindingKind,
    disposition: FindingDisposition,
    summary: &str,
    evidence: Vec<FindingEvidence>,
) -> TestManipulationFinding {
    TestManipulationFinding {
        kind,
        disposition,
        path: file.path.to_owned(),
        baseline_digest: file.baseline.map(content_digest),
        candidate_digest: file.candidate.map(content_digest),
        summary: summary.to_owned(),
        evidence,
    }
}

fn indexed_lines(content: &str) -> Vec<(usize, &str)> {
    content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.trim();
            (!trimmed.is_empty()).then_some((index + 1, trimmed))
        })
        .collect()
}

fn added_lines<'a>(
    old_lines: &[(usize, &'a str)],
    new_lines: &[(usize, &'a str)],
) -> Vec<(usize, &'a str)> {
    let mut remaining: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, line) in old_lines {
        *remaining.entry(line).or_default() += 1;
    }

    let mut added = Vec::new();
    for &(number, line) in new_lines {
        match remaining.get_mut(line) {
            Some(count) if *count > 0 => *count -= 1,
            _ => added.push((number, line)),
        }
    }
    added
}

fn first_matching<'a>(
    lines: &'a [(usize, &'a str)],
    predicate: fn(&str) -> bool,
) -> Option<(usize, &'a str)> {
    lines.iter().copied().find(|(_, line)| predicate(line))
}

fn matching_lines<'a>(
    lines: &'a [(usize, &'a str)],
    predicate: fn(&str) -> bool,
) -> Vec<(usize, &'a str)> {
    lines
        .iter()
        .copied()
        .filter(|(_, line)| predicate(line))
        .collect()
}

fn baseline_evidence((line, excerpt): (usize, &str)) -> FindingEvidence {
    FindingEvidence {
        side: EvidenceSide::Baseline,
        line,
        excerpt: bounded_excerpt(excerpt),
    }
}

fn candidate_evidence((line, excerpt): (usize, &str)) -> FindingEvidence {
    FindingEvidence {
        side: EvidenceSide::Candidate,
        line,
        excerpt: bounded_excerpt(excerpt),
    }
}

fn candidate_or_baseline_evidence(
    line: &(usize, &str),
    candidate_lines: &[(usize, &str)],
) -> FindingEvidence {
    if candidate_lines.contains(line) {
        candidate_evidence(*line)
    } else {
        baseline_evidence(*line)
    }
}

fn bounded_excerpt(excerpt: &str) -> String {
    const LIMIT: usize = 240;
    let mut chars = excerpt.chars();
    let bounded: String = chars.by_ref().take(LIMIT).collect();
    if chars.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

fn content_digest(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    lowercase_hex(&digest)
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn normalized(line: &str) -> String {
    line.to_ascii_lowercase().replace([' ', '\t'], "")
}

fn is_test_asset(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.contains("/test/")
        || path.contains("/tests/")
        || path.contains("/__tests__/")
        || path.ends_with("_test.rs")
        || path.ends_with("_test.py")
        || path.ends_with(".test.ts")
        || path.ends_with(".test.tsx")
        || path.ends_with(".test.js")
        || path.ends_with(".spec.ts")
        || path.ends_with(".spec.tsx")
        || path.ends_with(".spec.js")
        || is_snapshot_path(&path)
}

fn is_snapshot_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    Path::new(&path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("snap"))
        || path.contains("/__snapshots__/")
}

fn is_test_discovery_config(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with("jest.config.js")
        || path.ends_with("jest.config.ts")
        || path.ends_with("vitest.config.js")
        || path.ends_with("vitest.config.ts")
        || path.ends_with("pytest.ini")
        || path.ends_with("pyproject.toml")
        || path.ends_with(".cargo/nextest.toml")
        || path.ends_with("package.json")
}

fn is_disable_marker(line: &str) -> bool {
    let line = normalized(line);
    [
        ".skip(",
        "test.skip(",
        "describe.skip(",
        "#[ignore]",
        "@ignore",
        "pytest.mark.skip",
        "pytestmark=pytest.mark.skip",
        "xdescribe(",
        "xit(",
        "test.todo(",
        "disabled=true",
    ]
    .iter()
    .any(|marker| line.contains(marker))
}

fn is_assertion_line(line: &str) -> bool {
    let line = normalized(line);
    [
        "assert(",
        "assert!",
        "assert_eq!",
        "assert_ne!",
        "expect(",
        ".tobe(",
        ".toequal(",
        ".tomatch(",
        "should(",
        "require.",
        "verify(",
    ]
    .iter()
    .any(|marker| line.contains(marker))
}

fn is_fixture_or_mock_masking(line: &str) -> bool {
    let line = normalized(line);
    let forcing_success = [
        "mockreturnvalue(true)",
        "mockresolvedvalue(true)",
        "mockresolvedvalue({})",
        "mockimplementation(()=>true)",
        "returns(true)",
        "returnok(())",
        "always_pass",
        "bypass_validation",
    ];
    forcing_success.iter().any(|marker| line.contains(marker))
}

fn is_environment_bypass(line: &str) -> bool {
    let line = normalized(line);
    let environment = line.contains("process.env")
        || line.contains("std::env::var")
        || line.contains("os.environ")
        || line.contains("getenv(");
    let bypass = line.contains("skip")
        || line.contains("ignore")
        || line.contains("return;")
        || line.contains("returntrue")
        || line.contains("bypass");
    environment && bypass
}

fn coverage_thresholds(content: &str) -> BTreeMap<String, u64> {
    let mut thresholds = BTreeMap::new();
    let coverage_keys: BTreeSet<&str> = [
        "branches",
        "coverage",
        "fail-under",
        "functions",
        "lines",
        "minimum_coverage",
        "statements",
        "threshold",
    ]
    .into_iter()
    .collect();

    for line in content.lines() {
        let normalized_line = line
            .trim()
            .replace(['\"', '\'', ',', '{', '}'], " ")
            .replace([':', '='], " ");
        let fields: Vec<&str> = normalized_line.split_whitespace().collect();
        for pair in fields.windows(2) {
            let key = pair[0].to_ascii_lowercase();
            if coverage_keys.contains(key.as_str())
                && let Ok(value) = pair[1].trim_end_matches('%').parse::<u64>()
            {
                thresholds.insert(key, value);
            }
        }
    }
    thresholds
}
