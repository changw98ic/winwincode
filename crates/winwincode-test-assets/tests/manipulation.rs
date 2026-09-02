// SPDX-License-Identifier: Apache-2.0

use winwincode_test_assets::{
    ChangedFile, EvidenceSide, FindingDisposition, FindingKind, analyze_test_changes,
};

fn kinds(files: &[ChangedFile<'_>]) -> Vec<FindingKind> {
    analyze_test_changes(files)
        .into_iter()
        .map(|finding| finding.kind)
        .collect()
}

#[test]
fn unchanged_and_additive_tests_are_clean() {
    let files = [
        ChangedFile {
            path: "tests/unchanged.test.ts",
            baseline: Some("test('works', () => expect(run()).toBe(true))\n"),
            candidate: Some("test('works', () => expect(run()).toBe(true))\n"),
        },
        ChangedFile {
            path: "tests/additive.test.ts",
            baseline: Some("test('old', () => expect(old()).toBe(true))\n"),
            candidate: Some(
                "test('old', () => expect(old()).toBe(true))\n\
                 test('new', () => expect(newCase()).toBe(false))\n",
            ),
        },
    ];

    assert!(analyze_test_changes(&files).is_empty());
}

#[test]
fn deleted_disabled_and_weakened_tests_block() {
    let files = [
        ChangedFile {
            path: "tests/deleted.test.ts",
            baseline: Some("test('important', () => expect(run()).toBe(true))\n"),
            candidate: None,
        },
        ChangedFile {
            path: "tests/disabled.test.ts",
            baseline: Some("test('important', () => expect(run()).toBe(true))\n"),
            candidate: Some("test.skip('important', () => expect(run()).toBe(true))\n"),
        },
        ChangedFile {
            path: "crates/example/tests/weakened.rs",
            baseline: Some("assert_eq!(actual, expected);\nassert!(audit_complete);\n"),
            candidate: Some("assert_eq!(actual, expected);\n"),
        },
    ];

    let findings = analyze_test_changes(&files);
    assert!(findings.iter().any(|finding| {
        finding.kind == FindingKind::TestDeleted && finding.disposition == FindingDisposition::Block
    }));
    assert!(findings.iter().any(|finding| {
        finding.kind == FindingKind::TestDisabled
            && finding.disposition == FindingDisposition::Block
    }));
    assert!(findings.iter().any(|finding| {
        finding.kind == FindingKind::AssertionWeakened
            && finding.disposition == FindingDisposition::Block
    }));
}

#[test]
fn configuration_coverage_snapshot_mock_and_environment_changes_are_detected() {
    let large_snapshot = (0..25)
        .map(|index| format!("new snapshot line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let files = [
        ChangedFile {
            path: "vitest.config.ts",
            baseline: Some("export default { include: ['tests/**'] }\n"),
            candidate: Some("export default { exclude: ['tests/integration/**'] }\n"),
        },
        ChangedFile {
            path: "coverage.config",
            baseline: Some("branches: 90\nlines: 95\n"),
            candidate: Some("branches: 70\nlines: 95\n"),
        },
        ChangedFile {
            path: "tests/__snapshots__/view.snap",
            baseline: Some("old snapshot\n"),
            candidate: Some(&large_snapshot),
        },
        ChangedFile {
            path: "tests/service.test.ts",
            baseline: Some("expect(await service()).toBe(false)\n"),
            candidate: Some(
                "expect(await service()).toBe(false)\n\
                 dependency.mockResolvedValue(true)\n\
                 if (process.env.CI) return;\n",
            ),
        },
    ];

    let found = kinds(&files);
    for expected in [
        FindingKind::TestDiscoveryChanged,
        FindingKind::CoverageThresholdLowered,
        FindingKind::SnapshotBulkUpdate,
        FindingKind::FixtureOrMockMasking,
        FindingKind::EnvironmentBypass,
    ] {
        assert!(found.contains(&expected), "missing {expected:?}: {found:?}");
    }
}

#[test]
fn uncertain_removal_requires_review_and_binds_content_digests() {
    let files = [ChangedFile {
        path: "tests/scenario.test.ts",
        baseline: Some("test('scenario', scenario)\nconst legacyCase = input('legacy')\n"),
        candidate: Some("test('scenario', scenario)\n"),
    }];

    let findings = analyze_test_changes(&files);
    let finding = findings
        .iter()
        .find(|finding| finding.kind == FindingKind::ReviewRequired)
        .expect("uncertain removal should require review");
    assert_eq!(finding.disposition, FindingDisposition::Review);
    assert_eq!(finding.path, "tests/scenario.test.ts");
    assert_eq!(finding.baseline_digest.as_deref().map(str::len), Some(64));
    assert_eq!(finding.candidate_digest.as_deref().map(str::len), Some(64));
    assert_eq!(finding.evidence[0].side, EvidenceSide::Baseline);
    assert_eq!(finding.evidence[0].line, 2);
}

#[test]
fn finding_order_is_stable_across_input_order() {
    let first = ChangedFile {
        path: "tests/z.test.ts",
        baseline: Some("expect(z()).toBe(true)\n"),
        candidate: None,
    };
    let second = ChangedFile {
        path: "tests/a.test.ts",
        baseline: Some("expect(a()).toBe(true)\n"),
        candidate: Some("test.skip('a', () => expect(a()).toBe(true))\n"),
    };

    assert_eq!(
        analyze_test_changes(&[first, second]),
        analyze_test_changes(&[second, first])
    );
}

#[test]
fn every_rule_has_a_non_triggering_boundary_fixture() {
    let short_snapshot = (0..9)
        .map(|index| format!("candidate line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let fixtures = [
        ChangedFile {
            path: "src/deleted.rs",
            baseline: Some("fn helper() {}\n"),
            candidate: None,
        },
        ChangedFile {
            path: "tests/existing_skip.test.ts",
            baseline: Some("test.skip('known quarantine', check)\n"),
            candidate: Some("test.skip('known quarantine', check)\n"),
        },
        ChangedFile {
            path: "tests/replaced_assertion.test.ts",
            baseline: Some("expect(value).toBe(true)\n"),
            candidate: Some("expect(value).toEqual(true)\n"),
        },
        ChangedFile {
            path: "tests/__snapshots__/small.snap",
            baseline: Some(""),
            candidate: Some(&short_snapshot),
        },
        ChangedFile {
            path: "vitest.config.ts",
            baseline: Some("export default { include: ['tests/**'] }\n"),
            candidate: Some("export default { include: ['tests/**'] }\n"),
        },
        ChangedFile {
            path: "coverage.config",
            baseline: Some("branches: 80\nlines: 85\n"),
            candidate: Some("branches: 90\nlines: 85\n"),
        },
        ChangedFile {
            path: "tests/mock.test.ts",
            baseline: Some("expect(result).toBe(false)\n"),
            candidate: Some("expect(result).toBe(false)\ndependency.mockResolvedValue(false)\n"),
        },
        ChangedFile {
            path: "tests/environment.test.ts",
            baseline: Some("expect(result).toBe(true)\n"),
            candidate: Some("expect(result).toBe(true)\nconst mode = process.env.MODE\n"),
        },
    ];

    let found = kinds(&fixtures);
    for unexpected in [
        FindingKind::TestDeleted,
        FindingKind::TestDisabled,
        FindingKind::AssertionWeakened,
        FindingKind::SnapshotBulkUpdate,
        FindingKind::TestDiscoveryChanged,
        FindingKind::CoverageThresholdLowered,
        FindingKind::FixtureOrMockMasking,
        FindingKind::EnvironmentBypass,
    ] {
        assert!(
            !found.contains(&unexpected),
            "boundary fixture triggered {unexpected:?}: {found:?}"
        );
    }
}
