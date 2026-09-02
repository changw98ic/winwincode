// SPDX-License-Identifier: Apache-2.0

//! Canonical enterprise backup, restore, and governed deletion contracts.
//!
//! The crate coordinates database state, Artifact objects, and secret-reference
//! metadata without representing secret values. One manifest version is
//! accepted. Restore targets receive an authorization only after every required
//! component, tenant scope, consistency cut, dependency, count, and digest has
//! been verified.

mod capture;
mod deletion;
mod error;
mod manifest;
mod restore;

pub use deletion::{
    BackupDeletionProof, BackupDeletionReceipt, BackupDeletionResult, BackupDeletionStore,
    BackupDeletionStoreError, BackupRetentionCoordinator,
};
pub use error::{BackupError, BackupErrorKind};
pub use manifest::{
    BackupComponentKind, BackupComponentSnapshot, BackupDependency, BackupId, BackupManifest,
};
pub use restore::{
    RestoreActivation, RestoreCoordinator, RestoreEvidence, RestoreId, RestorePreparation,
    RestoreTarget, RestoreTargetError, VerifiedRestore,
};

pub(crate) const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
pub use capture::{
    BackupCaptureCoordinator, BackupSnapshotRequest, BackupSnapshotSource,
    BackupSnapshotSourceError,
};
