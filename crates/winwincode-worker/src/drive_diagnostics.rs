// SPDX-License-Identifier: Apache-2.0

//! Finite drive boundaries for propagated bridge errors and handled Core poll failures.

use std::{collections::HashSet, io::Write as _, path::Path};

use crate::WorkerErrorCode;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum DriveStage {
    Admission,
    CollectEffects,
    FlushBeforePoll,
    DeviceRead,
    DeviceIntake,
    PreparedTurn,
    DelegatedState,
    ObservationOpen,
    CorePoll,
    CollectPollEffects,
    CoreFact,
    FlushAfterPoll,
}

impl DriveStage {
    const fn name(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::CollectEffects => "collect_effects",
            Self::FlushBeforePoll => "flush_before_poll",
            Self::DeviceRead => "device_read",
            Self::DeviceIntake => "device_intake",
            Self::PreparedTurn => "prepared_turn",
            Self::DelegatedState => "delegated_state",
            Self::ObservationOpen => "observation_open",
            Self::CorePoll => "core_poll",
            Self::CollectPollEffects => "collect_poll_effects",
            Self::CoreFact => "core_fact",
            Self::FlushAfterPoll => "flush_after_poll",
        }
    }
}

#[derive(Default)]
pub(super) struct DriveDiagnostics {
    observed: HashSet<(DriveStage, DriveFailure)>,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum DriveFailure {
    UnexpectedMessage,
    CodexPollFailed,
}

impl DriveFailure {
    const fn name(self) -> &'static str {
        match self {
            Self::UnexpectedMessage => "UnexpectedMessage",
            Self::CodexPollFailed => "CodexPollFailed",
        }
    }
}

impl DriveDiagnostics {
    pub(super) fn record(&mut self, path: Option<&Path>, stage: DriveStage, code: WorkerErrorCode) {
        if code == WorkerErrorCode::UnexpectedMessage {
            self.record_failure(path, stage, DriveFailure::UnexpectedMessage);
        }
    }

    pub(super) fn record_core_poll_failure(&mut self, path: Option<&Path>) {
        self.record_failure(path, DriveStage::CorePoll, DriveFailure::CodexPollFailed);
    }

    fn record_failure(&mut self, path: Option<&Path>, stage: DriveStage, failure: DriveFailure) {
        let Some(path) = path else { return };
        if !self.observed.insert((stage, failure)) {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            // Only enum constants are emitted. Adapter errors, frames, identities
            // and Provider content never enter this diagnostic record.
            let _ = writeln!(
                log,
                "component=worker stage=drive_{} code={} ",
                stage.name(),
                failure.name()
            );
        }
    }
}
