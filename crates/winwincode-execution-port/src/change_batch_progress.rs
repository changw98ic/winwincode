// SPDX-License-Identifier: Apache-2.0

//! Deterministic ordering for canonical `ChangeBatch` progress events.
//!
//! The generated schema validates each event in isolation. This module owns
//! the cross-event rules: one immutable batch identity, a contiguous sequence,
//! and legal lifecycle transitions. Persistence remains caller-owned.

use std::fmt;

use crate::generated::{ChangeBatchIdentity, ChangeBatchProgressEvent, ChangeBatchProgressState};

/// Stateful validator for one `ChangeBatch` progress stream.
#[derive(Debug, Clone, Default)]
pub struct ChangeBatchProgressLedger {
    identity: Option<ChangeBatchIdentity>,
    sequence: i64,
    state: Option<ChangeBatchProgressState>,
}

impl ChangeBatchProgressLedger {
    /// Creates an empty ledger. The first accepted event must be `proposed`
    /// with sequence `1`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            identity: None,
            sequence: 0,
            state: None,
        }
    }

    /// Returns the last accepted sequence, or zero before the first event.
    #[must_use]
    pub const fn sequence(&self) -> i64 {
        self.sequence
    }

    /// Returns the last accepted lifecycle state.
    #[must_use]
    pub const fn state(&self) -> Option<&ChangeBatchProgressState> {
        self.state.as_ref()
    }

    /// Validates and records one event.
    ///
    /// # Errors
    ///
    /// Returns an error without changing the ledger when the event belongs to
    /// another batch, skips or repeats a sequence, starts outside `proposed`,
    /// or attempts an illegal transition.
    pub fn record(
        &mut self,
        event: &ChangeBatchProgressEvent,
    ) -> Result<(), ChangeBatchProgressError> {
        let expected_sequence = self.sequence + 1;
        if event.sequence != expected_sequence {
            return Err(ChangeBatchProgressError::UnexpectedSequence {
                expected: expected_sequence,
                actual: event.sequence,
            });
        }

        match (&self.identity, &self.state) {
            (None, None) => {
                if event.state != ChangeBatchProgressState::Proposed {
                    return Err(ChangeBatchProgressError::InvalidInitialState {
                        actual: event.state.clone(),
                    });
                }
            }
            (Some(identity), Some(previous)) => {
                if identity != &event.identity {
                    return Err(ChangeBatchProgressError::IdentityChanged);
                }
                if is_terminal(previous) {
                    return Err(ChangeBatchProgressError::TerminalState {
                        state: previous.clone(),
                    });
                }
                if !is_legal_transition(previous, &event.state) {
                    return Err(ChangeBatchProgressError::IllegalTransition {
                        from: previous.clone(),
                        to: event.state.clone(),
                    });
                }
            }
            _ => unreachable!("ChangeBatch progress ledger identity and state move together"),
        }

        if self.identity.is_none() {
            self.identity = Some(event.identity.clone());
        }
        self.sequence = event.sequence;
        self.state = Some(event.state.clone());
        Ok(())
    }
}

/// Validates a complete `ChangeBatch` progress stream in order.
///
/// Empty streams are valid because a proposal may not have been emitted yet.
///
/// # Errors
///
/// Returns the first lifecycle error and leaves no externally visible partial
/// state.
pub fn validate_change_batch_progress(
    events: &[ChangeBatchProgressEvent],
) -> Result<(), ChangeBatchProgressError> {
    let mut ledger = ChangeBatchProgressLedger::new();
    for event in events {
        ledger.record(event)?;
    }
    Ok(())
}

/// Cross-event failures for a `ChangeBatch` progress stream.
#[derive(Debug, Clone, PartialEq)]
pub enum ChangeBatchProgressError {
    /// The stream did not begin at the expected contiguous sequence.
    UnexpectedSequence { expected: i64, actual: i64 },
    /// The first lifecycle state was not `proposed`.
    InvalidInitialState { actual: ChangeBatchProgressState },
    /// An event changed any field of the replay-stable batch identity.
    IdentityChanged,
    /// A state followed another state for which it is not a legal successor.
    IllegalTransition {
        from: ChangeBatchProgressState,
        to: ChangeBatchProgressState,
    },
    /// A terminal state received a successor.
    TerminalState { state: ChangeBatchProgressState },
}

impl fmt::Display for ChangeBatchProgressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedSequence { expected, actual } => write!(
                formatter,
                "ChangeBatch progress sequence must be {expected}, received {actual}"
            ),
            Self::InvalidInitialState { .. } => {
                formatter.write_str("ChangeBatch progress must begin in proposed state")
            }
            Self::IdentityChanged => {
                formatter.write_str("ChangeBatch progress identity changed within one stream")
            }
            Self::IllegalTransition { .. } => {
                formatter.write_str("ChangeBatch progress transition is not allowed")
            }
            Self::TerminalState { .. } => {
                formatter.write_str("ChangeBatch progress terminal state has a successor")
            }
        }
    }
}

impl std::error::Error for ChangeBatchProgressError {}

const fn is_terminal(state: &ChangeBatchProgressState) -> bool {
    matches!(
        state,
        ChangeBatchProgressState::Accepted
            | ChangeBatchProgressState::RepairRequired
            | ChangeBatchProgressState::InfrastructureFailed
    )
}

// Keep the lifecycle table exhaustive over the generated enum. Adding a new
// canonical state intentionally fails to compile here until its transition
// row and column are reviewed.
const CHANGE_BATCH_PROGRESS_STATE_COUNT: usize = 13;

const fn state_index(state: &ChangeBatchProgressState) -> usize {
    match state {
        ChangeBatchProgressState::Proposed => 0,
        ChangeBatchProgressState::Authorized => 1,
        ChangeBatchProgressState::ApplyStarted => 2,
        ChangeBatchProgressState::Applied => 3,
        ChangeBatchProgressState::RollbackStarted => 4,
        ChangeBatchProgressState::RolledBack => 5,
        ChangeBatchProgressState::ValidationStarted => 6,
        ChangeBatchProgressState::ValidationCompleted => 7,
        ChangeBatchProgressState::ObservationRequested => 8,
        ChangeBatchProgressState::ObservationCompleted => 9,
        ChangeBatchProgressState::Accepted => 10,
        ChangeBatchProgressState::RepairRequired => 11,
        ChangeBatchProgressState::InfrastructureFailed => 12,
    }
}

#[rustfmt::skip]
const LEGAL_TRANSITIONS: [[bool; CHANGE_BATCH_PROGRESS_STATE_COUNT]; CHANGE_BATCH_PROGRESS_STATE_COUNT] = [
    // to: proposed authorized apply-started applied rollback-started rolled-back validation-started validation-completed observation-requested observation-completed accepted repair-required infrastructure-failed
    /* proposed              */ [false, true,  false, false, false, false, false, false, false, false, false, true,  true ],
    /* authorized            */ [false, false, true,  false, false, false, false, false, false, false, false, true,  true ],
    /* apply_started         */ [false, false, false, true,  true,  false, false, false, false, false, false, false, true ],
    /* applied               */ [false, false, false, false, true,  false, true,  false, false, false, false, false, true ],
    /* rollback_started      */ [false, false, false, false, false, true,  false, false, false, false, false, false, true ],
    /* rolled_back           */ [false, false, false, false, false, false, false, false, false, false, false, true,  true ],
    /* validation_started    */ [false, false, false, false, true,  false, false, true,  false, false, false, false, true ],
    /* validation_completed  */ [false, false, false, false, true,  false, false, false, true,  false, true,  true,  true ],
    /* observation_requested */ [false, false, false, false, false, false, false, false, false, true,  false, false, true ],
    /* observation_completed */ [false, false, false, false, true,  false, false, false, false, false, true,  true,  true ],
    /* accepted              */ [false, false, false, false, false, false, false, false, false, false, false, false, false],
    /* repair_required       */ [false, false, false, false, false, false, false, false, false, false, false, false, false],
    /* infrastructure_failed */ [false, false, false, false, false, false, false, false, false, false, false, false, false],
];

const fn is_legal_transition(
    from: &ChangeBatchProgressState,
    to: &ChangeBatchProgressState,
) -> bool {
    LEGAL_TRANSITIONS[state_index(from)][state_index(to)]
}

#[cfg(test)]
mod tests {
    use super::{is_legal_transition, is_terminal};
    use crate::generated::ChangeBatchProgressState;

    const STATES: [ChangeBatchProgressState; 13] = [
        ChangeBatchProgressState::Proposed,
        ChangeBatchProgressState::Authorized,
        ChangeBatchProgressState::ApplyStarted,
        ChangeBatchProgressState::Applied,
        ChangeBatchProgressState::RollbackStarted,
        ChangeBatchProgressState::RolledBack,
        ChangeBatchProgressState::ValidationStarted,
        ChangeBatchProgressState::ValidationCompleted,
        ChangeBatchProgressState::ObservationRequested,
        ChangeBatchProgressState::ObservationCompleted,
        ChangeBatchProgressState::Accepted,
        ChangeBatchProgressState::RepairRequired,
        ChangeBatchProgressState::InfrastructureFailed,
    ];

    const ALLOWED_TRANSITIONS: [(ChangeBatchProgressState, ChangeBatchProgressState); 30] = [
        (
            ChangeBatchProgressState::Proposed,
            ChangeBatchProgressState::Authorized,
        ),
        (
            ChangeBatchProgressState::Proposed,
            ChangeBatchProgressState::RepairRequired,
        ),
        (
            ChangeBatchProgressState::Proposed,
            ChangeBatchProgressState::InfrastructureFailed,
        ),
        (
            ChangeBatchProgressState::Authorized,
            ChangeBatchProgressState::ApplyStarted,
        ),
        (
            ChangeBatchProgressState::Authorized,
            ChangeBatchProgressState::RepairRequired,
        ),
        (
            ChangeBatchProgressState::Authorized,
            ChangeBatchProgressState::InfrastructureFailed,
        ),
        (
            ChangeBatchProgressState::ApplyStarted,
            ChangeBatchProgressState::Applied,
        ),
        (
            ChangeBatchProgressState::ApplyStarted,
            ChangeBatchProgressState::RollbackStarted,
        ),
        (
            ChangeBatchProgressState::ApplyStarted,
            ChangeBatchProgressState::InfrastructureFailed,
        ),
        (
            ChangeBatchProgressState::Applied,
            ChangeBatchProgressState::RollbackStarted,
        ),
        (
            ChangeBatchProgressState::Applied,
            ChangeBatchProgressState::ValidationStarted,
        ),
        (
            ChangeBatchProgressState::Applied,
            ChangeBatchProgressState::InfrastructureFailed,
        ),
        (
            ChangeBatchProgressState::RollbackStarted,
            ChangeBatchProgressState::RolledBack,
        ),
        (
            ChangeBatchProgressState::RollbackStarted,
            ChangeBatchProgressState::InfrastructureFailed,
        ),
        (
            ChangeBatchProgressState::RolledBack,
            ChangeBatchProgressState::RepairRequired,
        ),
        (
            ChangeBatchProgressState::RolledBack,
            ChangeBatchProgressState::InfrastructureFailed,
        ),
        (
            ChangeBatchProgressState::ValidationStarted,
            ChangeBatchProgressState::RollbackStarted,
        ),
        (
            ChangeBatchProgressState::ValidationStarted,
            ChangeBatchProgressState::ValidationCompleted,
        ),
        (
            ChangeBatchProgressState::ValidationStarted,
            ChangeBatchProgressState::InfrastructureFailed,
        ),
        (
            ChangeBatchProgressState::ValidationCompleted,
            ChangeBatchProgressState::RollbackStarted,
        ),
        (
            ChangeBatchProgressState::ValidationCompleted,
            ChangeBatchProgressState::ObservationRequested,
        ),
        (
            ChangeBatchProgressState::ValidationCompleted,
            ChangeBatchProgressState::Accepted,
        ),
        (
            ChangeBatchProgressState::ValidationCompleted,
            ChangeBatchProgressState::RepairRequired,
        ),
        (
            ChangeBatchProgressState::ValidationCompleted,
            ChangeBatchProgressState::InfrastructureFailed,
        ),
        (
            ChangeBatchProgressState::ObservationRequested,
            ChangeBatchProgressState::ObservationCompleted,
        ),
        (
            ChangeBatchProgressState::ObservationRequested,
            ChangeBatchProgressState::InfrastructureFailed,
        ),
        (
            ChangeBatchProgressState::ObservationCompleted,
            ChangeBatchProgressState::RollbackStarted,
        ),
        (
            ChangeBatchProgressState::ObservationCompleted,
            ChangeBatchProgressState::Accepted,
        ),
        (
            ChangeBatchProgressState::ObservationCompleted,
            ChangeBatchProgressState::RepairRequired,
        ),
        (
            ChangeBatchProgressState::ObservationCompleted,
            ChangeBatchProgressState::InfrastructureFailed,
        ),
    ];

    #[test]
    fn transition_matrix_is_exhaustive_over_all_canonical_states() {
        for from in &STATES {
            for to in &STATES {
                let expected = ALLOWED_TRANSITIONS.contains(&(from.clone(), to.clone()));
                assert_eq!(
                    is_legal_transition(from, to),
                    expected,
                    "unexpected transition decision: {from:?} -> {to:?}",
                );
            }
        }
    }

    #[test]
    fn terminal_states_are_exhaustive_and_have_no_successors() {
        for state in &STATES {
            let expected_terminal = matches!(
                state,
                ChangeBatchProgressState::Accepted
                    | ChangeBatchProgressState::RepairRequired
                    | ChangeBatchProgressState::InfrastructureFailed
            );
            assert_eq!(is_terminal(state), expected_terminal, "state: {state:?}");
            if expected_terminal {
                assert!(STATES.iter().all(|to| !is_legal_transition(state, to)));
            }
        }
    }
}
