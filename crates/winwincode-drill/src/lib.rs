// SPDX-License-Identifier: Apache-2.0

//! Deterministic rolling-upgrade and disaster-recovery drill runner.

mod disaster;
mod error;
mod identity;
mod lifecycle;
mod upgrade;

pub use disaster::{
    DisasterFaultPort, DisasterRecoveryEvidence, DisasterRecoveryPlan, DisasterRecoveryRunner,
    DisasterScenario, FailureObservation, FaultPortError, RecoveryObservation,
    RecoveryObservationPort, RecoveryObservationPortError,
};
pub use error::{DrillError, DrillErrorKind};
pub use identity::DrillId;
pub use lifecycle::{
    ControlPlaneLifecycleSnapshot, DeploymentDrainAuthority, DrainAuthorityError, LifecycleState,
};
pub use upgrade::{
    DeploymentRelease, RollingUpgradeBackup, RollingUpgradeBackupPort,
    RollingUpgradeBackupPortError, RollingUpgradeEvidence, RollingUpgradePlan, RollingUpgradePort,
    RollingUpgradePortError, RollingUpgradeResult, RollingUpgradeRunner,
};

pub(crate) const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
