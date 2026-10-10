// SPDX-License-Identifier: Apache-2.0
use super::*;
use codex_state::ToolAttemptReceipt;
use codex_state::ToolCoalescingPermission;
use codex_state::ToolDependencySnapshot;
use codex_state::ToolExecutionStatus;
use codex_state::ToolOutputDelivery;
use codex_state::ToolOutputDisposition;
use codex_state::ToolProgressFact;
use codex_state::ToolRequestIdentity;
use codex_state::ToolRequestResolution;
use codex_state::ToolReusePermission;
use codex_state::ToolSharingFact;
use codex_state::ToolSharingKind;
use pretty_assertions::assert_eq;
fn request(
    sequence: i64,
    operation: &str,
    parent: Option<&str>,
    cell: Option<&str>,
) -> ToolRuntimeEvent {
    ToolRuntimeEvent {
        sequence,
        fact: ToolRuntimeFact::Request(ToolExecutionFact {
            schema_version: 1,
            request_sequence: sequence,
            request: ToolRequestIdentity {
                thread_id: "thread".into(),
                logical_id: format!("call-{sequence}"),
                turn_id: "turn".into(),
                scope_id: "scope".into(),
                cell_id: cell.map(str::to_owned),
                parent_call_id: parent.map(str::to_owned),
                tool_name: "mcp.fixture.check".into(),
                source: "code_mode".into(),
                binding: operation.into(),
            },
            resolution: ToolRequestResolution::Observed,
            attempt: Some(ToolAttemptReceipt {
                attempt_id: format!("attempt-{sequence}"),
                owner_id: "owner".into(),
                operation_digest: operation.into(),
                execution: ToolExecutionStatus::Completed,
                disposition: ToolOutputDisposition::Accepted,
                delivery: ToolOutputDelivery::Offered,
                revision: 4,
                execution_result_retained: true,
                accepted_result_retained: true,
            }),
        }),
    }
}
#[test]
fn repeated_requests_produce_stable_evidence_but_transport_replay_does_not_count() {
    let mut detector = Detector::default();
    detector.consume(
        (1..=5)
            .map(|n| request(n, "same", Some("parent"), Some("cell")))
            .collect(),
    );
    detector.consume(vec![request(5, "same", Some("parent"), Some("cell"))]);
    assert!(detector.diagnose("thread", "owner").is_empty());
    detector.consume(vec![request(6, "same", Some("parent"), Some("cell"))]);
    let first = detector.diagnose("thread", "owner");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].kind, ToolDiagnosticKind::RepeatedOperation);
    assert_eq!(first[0].progress_source_sequence, None);
    detector.consume(vec![request(7, "same", Some("parent"), Some("cell"))]);
    let second = detector.diagnose("thread", "owner");
    assert_eq!(first[0].diagnostic_id, second[0].diagnostic_id);
    assert!(second[0].evidence_version > first[0].evidence_version);
}

#[test]
fn shared_call_evidence_keeps_the_operation_that_triggered_the_diagnosis() {
    for kind in [ToolSharingKind::Reuse, ToolSharingKind::Merged] {
        let mut detector = Detector::default();
        let operation = "a".repeat(64);
        detector.consume(vec![request(1, &operation, Some("parent"), Some("cell"))]);
        for sequence in 2..=6 {
            let mut event = request(sequence, "same-input", Some("parent"), Some("cell"));
            event.sequence = sequence * 2;
            if let ToolRuntimeFact::Request(fact) = &mut event.fact {
                fact.attempt = None;
            }
            detector.consume(vec![
                event,
                ToolRuntimeEvent {
                    sequence: sequence * 2 + 1,
                    fact: ToolRuntimeFact::Sharing(ToolSharingFact {
                        schema_version: 1,
                        thread_id: "thread".into(),
                        request_sequence: sequence,
                        source_request_sequence: 1,
                        source_attempt_id: "attempt-1".into(),
                        operation_digest: operation.clone(),
                        snapshot: ToolDependencySnapshot {
                            policy_revision: "1".repeat(64),
                            dependency_digest: "2".repeat(64),
                            account_scope_digest: "3".repeat(64),
                            session_scope_digest: "4".repeat(64),
                            validity_epoch: "5".repeat(64),
                            reuse: ToolReusePermission::ImmutableValue,
                            coalescing: ToolCoalescingPermission::SharedRead,
                        },
                        kind,
                        disposition: ToolOutputDisposition::Accepted,
                        delivery: ToolOutputDelivery::Offered,
                        cancelled: false,
                    }),
                },
            ]);
        }
        let diagnoses = detector.diagnose("thread", "owner");
        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].kind, ToolDiagnosticKind::RepeatedOperation);
        assert_eq!(diagnoses[0].evidence.len(), 4);
        assert!(diagnoses[0].evidence.iter().all(|call| {
            call.request_sequence >= 3
                && call.operation_digest == operation
                && call.parent_call_id.as_deref() == Some("parent")
                && call.cell_id.as_deref() == Some("cell")
        }));
    }
}

#[test]
fn branch_expansion_evidence_compares_distinct_parent_calls() {
    let mut detector = Detector::default();
    detector.consume(
        (1..=12)
            .map(|sequence| {
                let branch = (sequence - 1) / 4;
                request(
                    sequence,
                    "same",
                    Some(&format!("parent-{branch}")),
                    Some(&format!("cell-{branch}")),
                )
            })
            .collect(),
    );
    let diagnoses = detector.diagnose("thread", "owner");
    let branch = diagnoses
        .iter()
        .find(|diagnostic| diagnostic.kind == ToolDiagnosticKind::BranchExpansion)
        .unwrap();
    assert_eq!(branch.evidence.len(), 3);
    assert_eq!(
        branch
            .evidence
            .iter()
            .map(|call| call.request_sequence)
            .collect::<Vec<_>>(),
        [4, 8, 12]
    );
    let visible = branch.evidence.iter().rev().take(2).collect::<Vec<_>>();
    assert_ne!(visible[0].parent_call_id, visible[1].parent_call_id);
    assert_ne!(visible[0].cell_id, visible[1].cell_id);
    let repeated = diagnoses
        .iter()
        .find(|diagnostic| diagnostic.kind == ToolDiagnosticKind::RepeatedOperation)
        .unwrap();
    assert_eq!(
        repeated
            .evidence
            .iter()
            .map(|call| call.request_sequence)
            .collect::<Vec<_>>(),
        [9, 10, 11, 12]
    );
}
#[test]
fn alternating_cycles_and_branch_expansion_are_separate_questions() {
    let mut detector = Detector::default();
    detector.consume(
        (1..=6)
            .map(|n| request(n, if n % 2 == 0 { "a" } else { "b" }, Some("parent"), None))
            .collect(),
    );
    let alternating = detector
        .diagnose("thread", "owner")
        .into_iter()
        .find(|diagnostic| diagnostic.kind == ToolDiagnosticKind::AlternatingCycle)
        .unwrap();
    detector.consume(vec![request(7, "b", Some("parent"), None)]);
    let shifted = detector
        .diagnose("thread", "owner")
        .into_iter()
        .find(|diagnostic| diagnostic.kind == ToolDiagnosticKind::AlternatingCycle)
        .unwrap();
    assert_eq!(alternating.diagnostic_id, shifted.diagnostic_id);
    detector.consume(
        (7..=12)
            .map(|n| {
                request(
                    n,
                    "a",
                    Some(if n % 2 == 0 { "parent2" } else { "parent3" }),
                    None,
                )
            })
            .collect(),
    );
    assert!(
        detector
            .diagnose("thread", "owner")
            .iter()
            .any(|diagnostic| diagnostic.kind == ToolDiagnosticKind::BranchExpansion)
    );
}
#[test]
fn independent_parallel_operations_and_verified_progress_have_distinct_meanings() {
    let mut detector = Detector::default();
    detector.consume(
        (1..=20)
            .map(|n| {
                request(
                    n,
                    &format!("independent-{n}"),
                    Some(&format!("parent-{n}")),
                    None,
                )
            })
            .collect(),
    );
    assert!(detector.diagnose("thread", "owner").is_empty());
    detector.consume((21..=26).map(|n| request(n, "same", None, None)).collect());
    let before = detector.diagnose("thread", "owner")[0]
        .diagnostic_id
        .clone();
    detector.consume(vec![ToolRuntimeEvent {
        sequence: 27,
        fact: ToolRuntimeFact::Progress(ToolProgressFact {
            schema_version: 1,
            thread_id: "thread".into(),
            request_sequence: 26,
            evidence_digest: "trusted-receipt".into(),
            source: "adapter".into(),
        }),
    }]);
    assert!(detector.diagnose("thread", "owner").is_empty());
    detector.consume((28..=33).map(|n| request(n, "same", None, None)).collect());
    let after = detector.diagnose("thread", "owner");
    assert_eq!(after[0].progress_source_sequence, Some(27));
    assert_ne!(after[0].diagnostic_id, before);
}
#[test]
fn only_trusted_live_cell_wait_edges_can_form_a_cycle() {
    let mut detector = Detector::default();
    for (seq, cell) in [(1, "a"), (2, "b")] {
        detector.consume(vec![ToolRuntimeEvent {
            sequence: seq,
            fact: ToolRuntimeFact::Cell(ToolCellFact {
                schema_version: 1,
                sequence: seq,
                thread_id: "thread".into(),
                parent_request_sequence: seq,
                cell_id: cell.into(),
                scope_id: "scope".into(),
                owner_id: "owner".into(),
                lifecycle: ToolCellLifecycle::Live,
                revision: 1,
            }),
        }]);
    }
    detector.consume(vec![
        request(3, "wait-b", None, Some("a")),
        request(4, "wait-a", None, Some("b")),
    ]);
    let wait = |request, target| ToolRuntimeEvent {
        sequence: request + 2,
        fact: ToolRuntimeFact::Wait(ToolWaitFact {
            schema_version: 1,
            thread_id: "thread".into(),
            waiter_request_sequence: request,
            target_cell_sequence: target,
            owner_id: "owner".into(),
            state: ToolWaitState::Waiting,
            revision: 1,
        }),
    };
    detector.consume(vec![wait(3, 2)]);
    assert!(detector.diagnose("thread", "owner").is_empty());
    detector.consume(vec![wait(4, 1)]);
    let diagnostics = detector.diagnose("thread", "owner");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].kind, ToolDiagnosticKind::WaitCycle);
    assert!(detector.diagnose("thread", "different-owner").is_empty());
    let mut settled = wait(4, 1);
    if let ToolRuntimeFact::Wait(fact) = &mut settled.fact {
        fact.state = ToolWaitState::Settled;
    }
    detector.consume(vec![settled]);
    assert!(detector.diagnose("thread", "owner").is_empty());
}
