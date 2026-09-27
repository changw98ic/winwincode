// SPDX-License-Identifier: Apache-2.0

//! Strict runtime identity for immutable verification snapshots.

use std::{error::Error, fmt};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use super::{
    CandidateId, GitObjectId, SchemaVersion, Sha256Digest, Snapshot, SnapshotId,
    VerificationSessionId, WorkRunId, is_canonical_prefixed_id,
};

/// Why a generated Snapshot cannot be promoted into a trusted runtime value.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SnapshotIdentityError {
    InvalidIdentity,
    InvalidCodeIdentity,
    Mutable,
}

impl fmt::Display for SnapshotIdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidIdentity => "Snapshot identity is not canonical",
            Self::InvalidCodeIdentity => "Snapshot code identity is malformed",
            Self::Mutable => "Snapshot must be immutable",
        })
    }
}

impl Error for SnapshotIdentityError {}

/// A schema-shaped Snapshot whose runtime values passed strict identity checks.
///
/// Construction accepts an already allocated identity. This module never
/// allocates a `SnapshotId`, and the value exposes no mutation API.
///
/// ```compile_fail
/// use winwincode_domain::{CanonicalSnapshot, GitObjectId};
/// fn rewrite(snapshot: &mut CanonicalSnapshot) {
///     snapshot.as_contract().candidate_commit_id = GitObjectId("f".repeat(40));
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CanonicalSnapshot(Snapshot);

// All validated Snapshot fields have reflexive equality (no floating-point values).
impl Eq for CanonicalSnapshot {}

impl CanonicalSnapshot {
    #[must_use]
    pub fn snapshot_id(&self) -> &SnapshotId {
        &self.0.snapshot_id
    }

    #[must_use]
    pub fn candidate_id(&self) -> &CandidateId {
        &self.0.candidate_id
    }

    #[must_use]
    pub fn work_run_id(&self) -> &WorkRunId {
        &self.0.work_run_id
    }

    #[must_use]
    pub fn as_contract(&self) -> &Snapshot {
        &self.0
    }

    #[must_use]
    pub fn into_contract(self) -> Snapshot {
        self.0
    }

    #[must_use]
    pub fn verification_binding(
        &self,
        verification_session_id: VerificationSessionId,
    ) -> SnapshotVerificationBinding {
        SnapshotVerificationBinding {
            snapshot_id: self.0.snapshot_id.clone(),
            candidate_id: self.0.candidate_id.clone(),
            work_run_id: self.0.work_run_id.clone(),
            verification_session_id,
        }
    }
}

impl TryFrom<Snapshot> for CanonicalSnapshot {
    type Error = SnapshotIdentityError;

    fn try_from(value: Snapshot) -> Result<Self, Self::Error> {
        validate_contract(&value)?;
        Ok(Self(value))
    }
}

impl<'de> Deserialize<'de> for CanonicalSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Snapshot::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotVerificationBinding {
    #[serde(rename = "snapshotId")]
    pub snapshot_id: SnapshotId,
    #[serde(rename = "candidateId")]
    pub candidate_id: CandidateId,
    #[serde(rename = "workRunId")]
    pub work_run_id: WorkRunId,
    #[serde(rename = "verificationSessionId")]
    pub verification_session_id: VerificationSessionId,
}

/// Why one runtime record does not resolve to its bound product Snapshot.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SnapshotBindingError {
    Malformed,
    SnapshotMismatch { expected: String, actual: String },
    CandidateMismatch { expected: String, actual: String },
    WorkRunMismatch { expected: String, actual: String },
    VerificationSessionMismatch { expected: String, actual: String },
}

impl fmt::Display for SnapshotBindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "verification Snapshot binding is malformed",
            Self::SnapshotMismatch { .. } => "verification Snapshot mismatch",
            Self::CandidateMismatch { .. } => "verification Candidate mismatch",
            Self::WorkRunMismatch { .. } => "verification WorkRun mismatch",
            Self::VerificationSessionMismatch { .. } => "verification Session mismatch",
        })
    }
}

impl Error for SnapshotBindingError {}

impl SnapshotVerificationBinding {
    /// # Errors
    /// Rejects any identity that differs from the bound verification input.
    pub fn accept(
        &self,
        snapshot_id: &SnapshotId,
        candidate_id: &CandidateId,
        work_run_id: &WorkRunId,
        verification_session_id: &VerificationSessionId,
    ) -> Result<(), SnapshotBindingError> {
        if self.snapshot_id != *snapshot_id {
            return Err(SnapshotBindingError::SnapshotMismatch {
                expected: self.snapshot_id.0.clone(),
                actual: snapshot_id.0.clone(),
            });
        }
        if self.candidate_id != *candidate_id {
            return Err(SnapshotBindingError::CandidateMismatch {
                expected: self.candidate_id.0.clone(),
                actual: candidate_id.0.clone(),
            });
        }
        if self.work_run_id != *work_run_id {
            return Err(SnapshotBindingError::WorkRunMismatch {
                expected: self.work_run_id.0.clone(),
                actual: work_run_id.0.clone(),
            });
        }
        if self.verification_session_id != *verification_session_id {
            return Err(SnapshotBindingError::VerificationSessionMismatch {
                expected: self.verification_session_id.0.clone(),
                actual: verification_session_id.0.clone(),
            });
        }
        Ok(())
    }

    /// # Errors
    /// Rejects malformed JSON and non-canonical identities.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, SnapshotBindingError> {
        let binding: Self =
            serde_json::from_value(value.clone()).map_err(|_| SnapshotBindingError::Malformed)?;
        if !is_canonical_prefixed_id(&binding.snapshot_id.0, "snap_")
            || !is_canonical_prefixed_id(&binding.candidate_id.0, "cnd_")
            || !is_canonical_prefixed_id(&binding.work_run_id.0, "wrn_")
            || !is_canonical_prefixed_id(&binding.verification_session_id.0, "vsn_")
        {
            return Err(SnapshotBindingError::Malformed);
        }
        Ok(binding)
    }
}

fn validate_contract(value: &Snapshot) -> Result<(), SnapshotIdentityError> {
    if value.schema_version != SchemaVersion::WinwincodeV1 || !value.immutable {
        return Err(SnapshotIdentityError::Mutable);
    }
    if !is_canonical_prefixed_id(&value.snapshot_id.0, "snap_")
        || !is_canonical_prefixed_id(&value.candidate_id.0, "cnd_")
        || !is_canonical_prefixed_id(&value.work_run_id.0, "wrn_")
        || !is_canonical_prefixed_id(&value.repository_id.0, "rep_")
    {
        return Err(SnapshotIdentityError::InvalidIdentity);
    }
    if value.created_at_millis < 0
        || !git_object(&value.base_commit_id)
        || !git_object(&value.base_tree_id)
        || !git_object(&value.candidate_commit_id)
        || !git_object(&value.candidate_tree_id)
        || !digest(&value.diff_sha256)
        || !digest(&value.content_digest)
        || !digest(&value.validation_seal)
    {
        return Err(SnapshotIdentityError::InvalidCodeIdentity);
    }
    Ok(())
}

fn git_object(value: &GitObjectId) -> bool {
    matches!(value.0.len(), 40 | 64)
        && value
            .0
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn digest(value: &Sha256Digest) -> bool {
    value.0.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// Recomputes the identity seal without permitting identity mutation.
#[must_use]
pub fn verify_snapshot_seal(snapshot: &CanonicalSnapshot) -> bool {
    seal_snapshot(&snapshot.0) == snapshot.0.validation_seal
}

/// Computes the immutable identity seal for one generated `Snapshot` contract.
#[must_use]
pub fn seal_snapshot(snapshot: &Snapshot) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.snapshot.product.v1\0");
    for field in [
        snapshot.snapshot_id.0.as_bytes(),
        snapshot.candidate_id.0.as_bytes(),
        snapshot.work_run_id.0.as_bytes(),
        snapshot.repository_id.0.as_bytes(),
        snapshot.base_commit_id.0.as_bytes(),
        snapshot.base_tree_id.0.as_bytes(),
        snapshot.candidate_commit_id.0.as_bytes(),
        snapshot.candidate_tree_id.0.as_bytes(),
        snapshot.diff_sha256.0.as_bytes(),
        snapshot.content_digest.0.as_bytes(),
        b"winwincode/v1",
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    hasher.update(snapshot.created_at_millis.to_be_bytes());
    Sha256Digest(format!("sha256:{:x}", hasher.finalize()))
}
