// SPDX-License-Identifier: Apache-2.0

use winwincode_domain::WorkspaceRevision;

use winwincode_domain::{
    ChangeBatchId, CodexThreadId, ExecutionJobId, FencingToken, Instant, LeaseId, ProductSessionId,
    RepositoryId, SessionIdentity, Sha256Digest, WorkerSessionId,
};
use winwincode_execution_port::{
    change_batch_progress::{
        ChangeBatchProgressError, ChangeBatchProgressLedger, validate_change_batch_progress,
    },
    generated::{ChangeBatchIdentity, ChangeBatchProgressEvent, ChangeBatchProgressState},
};

fn identity() -> ChangeBatchIdentity {
    ChangeBatchIdentity {
        attempt: 1,
        batch_id: ChangeBatchId(format!("sha256:{}", "0".repeat(64))),
        call_id: None,
        fencing_token: FencingToken("1".to_owned()),
        job_id: ExecutionJobId("job_00000000000000000000000000".to_owned()),
        lease_id: LeaseId("lse_00000000000000000000000000".to_owned()),
        patch_digest: Sha256Digest(format!("sha256:{}", "1".repeat(64))),
        repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
        run_key: "run-key-1".to_owned(),
        session_identity: SessionIdentity {
            product_session_id: ProductSessionId("psn_00000000000000000000000000".to_owned()),
            stage_run_id: None,
            worker_session_id: WorkerSessionId("wsn_00000000000000000000000000".to_owned()),
            codex_thread_id: CodexThreadId("cdx_00000000000000000000000000".to_owned()),
        },
        turn_id: "turn-1".to_owned(),
        workspace_revision: WorkspaceRevision(
            "git-tree:0000000000000000000000000000000000000000".to_owned(),
        ),
    }
}

fn event(sequence: i64, state: ChangeBatchProgressState) -> ChangeBatchProgressEvent {
    ChangeBatchProgressEvent {
        artifact_refs: Vec::new(),
        identity: identity(),
        occurred_at: Instant(format!("2026-08-31T12:00:{sequence:02}.000Z")),
        sequence,
        state,
        summary: "bounded lifecycle evidence".to_owned(),
    }
}

#[test]
fn accepts_apply_validate_observe_and_accept_path() {
    let states = [
        ChangeBatchProgressState::Proposed,
        ChangeBatchProgressState::Authorized,
        ChangeBatchProgressState::ApplyStarted,
        ChangeBatchProgressState::Applied,
        ChangeBatchProgressState::ValidationStarted,
        ChangeBatchProgressState::ValidationCompleted,
        ChangeBatchProgressState::ObservationRequested,
        ChangeBatchProgressState::ObservationCompleted,
        ChangeBatchProgressState::Accepted,
    ];
    let events = (1_i64..)
        .zip(states)
        .map(|(sequence, state)| event(sequence, state))
        .collect::<Vec<_>>();

    validate_change_batch_progress(&events).expect("canonical lifecycle");
}

#[test]
fn accepts_rollback_to_repair_path() {
    let states = [
        ChangeBatchProgressState::Proposed,
        ChangeBatchProgressState::Authorized,
        ChangeBatchProgressState::ApplyStarted,
        ChangeBatchProgressState::Applied,
        ChangeBatchProgressState::ValidationStarted,
        ChangeBatchProgressState::RollbackStarted,
        ChangeBatchProgressState::RolledBack,
        ChangeBatchProgressState::RepairRequired,
    ];
    let events = (1_i64..)
        .zip(states)
        .map(|(sequence, state)| event(sequence, state))
        .collect::<Vec<_>>();

    validate_change_batch_progress(&events).expect("canonical rollback lifecycle");
}

#[test]
fn rejects_sequence_gap_or_repeat_without_mutating_the_ledger() {
    let mut ledger = ChangeBatchProgressLedger::new();
    ledger
        .record(&event(1, ChangeBatchProgressState::Proposed))
        .expect("proposal");

    let repeated = ledger
        .record(&event(1, ChangeBatchProgressState::Authorized))
        .expect_err("repeat must be rejected");
    assert_eq!(
        repeated,
        ChangeBatchProgressError::UnexpectedSequence {
            expected: 2,
            actual: 1
        }
    );

    let error = ledger
        .record(&event(3, ChangeBatchProgressState::Authorized))
        .expect_err("gap must be rejected");
    assert_eq!(
        error,
        ChangeBatchProgressError::UnexpectedSequence {
            expected: 2,
            actual: 3
        }
    );
    assert_eq!(ledger.sequence(), 1);
    assert_eq!(ledger.state(), Some(&ChangeBatchProgressState::Proposed));
}

#[test]
fn rejects_identity_change_illegal_transition_and_terminal_successor() {
    let mut ledger = ChangeBatchProgressLedger::new();
    ledger
        .record(&event(1, ChangeBatchProgressState::Proposed))
        .expect("proposal");

    let mut changed = event(2, ChangeBatchProgressState::Authorized);
    changed.identity.workspace_revision =
        WorkspaceRevision("git-tree:cccccccccccccccccccccccccccccccccccccccc".to_owned());
    assert_eq!(
        ledger.record(&changed),
        Err(ChangeBatchProgressError::IdentityChanged)
    );

    assert!(matches!(
        ledger.record(&event(2, ChangeBatchProgressState::Applied)),
        Err(ChangeBatchProgressError::IllegalTransition { .. })
    ));

    ledger
        .record(&event(2, ChangeBatchProgressState::RepairRequired))
        .expect("proposal can require repair");
    assert!(matches!(
        ledger.record(&event(3, ChangeBatchProgressState::Authorized)),
        Err(ChangeBatchProgressError::TerminalState { .. })
    ));
}

#[test]
fn every_terminal_state_rejects_every_successor() {
    let terminal_paths = [
        vec![
            ChangeBatchProgressState::Proposed,
            ChangeBatchProgressState::Authorized,
            ChangeBatchProgressState::ApplyStarted,
            ChangeBatchProgressState::Applied,
            ChangeBatchProgressState::ValidationStarted,
            ChangeBatchProgressState::ValidationCompleted,
            ChangeBatchProgressState::Accepted,
        ],
        vec![
            ChangeBatchProgressState::Proposed,
            ChangeBatchProgressState::RepairRequired,
        ],
        vec![
            ChangeBatchProgressState::Proposed,
            ChangeBatchProgressState::InfrastructureFailed,
        ],
    ];

    for path in terminal_paths {
        let mut ledger = ChangeBatchProgressLedger::new();
        for (sequence, state) in (1_i64..).zip(path) {
            ledger
                .record(&event(sequence, state))
                .expect("valid terminal path");
        }

        let terminal = ledger.state().expect("terminal state").clone();
        assert!(matches!(
            ledger.record(&event(
                ledger.sequence() + 1,
                ChangeBatchProgressState::InfrastructureFailed,
            )),
            Err(ChangeBatchProgressError::TerminalState { state }) if state == terminal
        ));
    }
}

#[test]
fn rejects_non_proposed_initial_state() {
    assert!(matches!(
        validate_change_batch_progress(&[event(1, ChangeBatchProgressState::Authorized)]),
        Err(ChangeBatchProgressError::InvalidInitialState { .. })
    ));
}
