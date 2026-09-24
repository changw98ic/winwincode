// SPDX-License-Identifier: Apache-2.0

//! Immutable code identity one verification run is bound to.
//!
//! A `Snapshot` is created once from a frozen Candidate, before any
//! verification starts. It is never modified afterwards; a changed code
//! identity is a new Snapshot.

use std::{fmt, sync::atomic::{AtomicU64, Ordering}};

use sha2::{Digest, Sha256};
use winwincode_domain::{RepositoryId, SnapshotId, WorkRunId, is_canonical_prefixed_id};

/// Failure while assembling one Snapshot.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SnapshotError {
    /// A required code identity field is missing or malformed.
    Incomplete,
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("snapshot is missing required code identity")
    }
}

impl std::error::Error for SnapshotError {}

/// One immutable code identity under verification.
///
/// The `snapshot_id` field mirrors the canonical contract's `snapshotId`.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    snapshot_id: SnapshotId,
    candidate_ref: String,
    work_run_id: WorkRunId,
    repository_id: RepositoryId,
    base_commit_id: String,
    base_tree_id: String,
    candidate_commit_id: String,
    candidate_tree_id: String,
    diff_sha256: String,
    content_digest: String,
    created_at_millis: u64,
    validation_seal: String,
}

impl Snapshot {
    /// Returns the stable snapshot identity in its canonical string form.
    ///
    /// The generated [`SnapshotId`] newtype has no accessors of its own, so the
    /// sealed string form it serialises as is returned directly.
    #[must_use]
    pub fn snapshot_id(&self) -> &String {
        &self.snapshot_id.0
    }

    /// Returns the repository this snapshot belongs to, in its canonical
    /// string form.
    #[must_use]
    pub fn repository_id(&self) -> &String {
        &self.repository_id.0
    }

    /// Returns the base commit the candidate is measured against.
    #[must_use]
    pub fn base_commit_id(&self) -> &str {
        &self.base_commit_id
    }

    /// Returns the base tree the candidate is measured against.
    #[must_use]
    pub fn base_tree_id(&self) -> &str {
        &self.base_tree_id
    }

    /// Returns the exact candidate commit under verification.
    #[must_use]
    pub fn candidate_commit_id(&self) -> &str {
        &self.candidate_commit_id
    }

    /// Returns the exact candidate tree under verification.
    #[must_use]
    pub fn candidate_tree_id(&self) -> &str {
        &self.candidate_tree_id
    }

    /// Returns the fingerprint of the change between base and candidate.
    #[must_use]
    pub fn diff_sha256(&self) -> &str {
        &self.diff_sha256
    }

    /// Returns the fingerprint of the canonical manifest bytes.
    #[must_use]
    pub fn content_digest(&self) -> &str {
        &self.content_digest
    }

    /// Returns the seal over the complete code identity.
    #[must_use]
    pub fn validation_seal(&self) -> &str {
        &self.validation_seal
    }

    /// Returns the moment this snapshot was created.
    #[must_use]
    pub const fn created_at_millis(&self) -> u64 {
        self.created_at_millis
    }

    /// Test-only: simulates a rewritten identity to prove the seal catches it.
    ///
    /// This seam is `pub` rather than `cfg`-gated because an integration test
    /// links this crate without `cfg(test)` and without the `test-support`
    /// feature, so `#[cfg(any(test, feature = "test-support"))]` would hide it
    /// from `tests/snapshot_binding.rs`. Production paths never call it; only
    /// the invariant tests do, and any real identity change is a new Snapshot.
    pub fn force_candidate_commit_for_test(&mut self, commit_id: &str) {
        commit_id.clone_into(&mut self.candidate_commit_id);
    }
}

/// Recomputes the seal and compares it with the stored one.
#[must_use]
pub fn verify_seal(snapshot: &Snapshot) -> bool {
    seal_fields(snapshot) == snapshot.validation_seal
}

/// Hashes the complete code identity of one snapshot into its seal form.
///
/// The seal deliberately excludes `validation_seal` itself, so it can be
/// recomputed from any snapshot — sealed or half-assembled — and compared.
fn seal_fields(snapshot: &Snapshot) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.snapshot.v1\0");
    for field in [
        snapshot.candidate_ref.as_bytes(),
        snapshot.work_run_id.0.as_bytes(),
        snapshot.repository_id.0.as_bytes(),
        snapshot.base_commit_id.as_bytes(),
        snapshot.base_tree_id.as_bytes(),
        snapshot.candidate_commit_id.as_bytes(),
        snapshot.candidate_tree_id.as_bytes(),
        snapshot.diff_sha256.as_bytes(),
        snapshot.content_digest.as_bytes(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    hasher.update(snapshot.created_at_millis.to_be_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// Assembles one Snapshot from an exact frozen Candidate.
#[derive(Clone, Debug)]
pub struct SnapshotBuilder {
    candidate_ref: String,
    work_run_id: String,
    repository_id: String,
    base_commit_id: Option<String>,
    base_tree_id: Option<String>,
    candidate_commit_id: Option<String>,
    candidate_tree_id: Option<String>,
    diff_sha256: Option<String>,
    content_digest: Option<String>,
    created_at_millis: Option<u64>,
}

impl SnapshotBuilder {
    /// Starts a builder bound to one candidate, work run and repository.
    ///
    /// The candidate identity is the job's own `candidate_ref` string, taken
    /// verbatim; nothing in the Worker inputs produces a `cnd_` newtype.
    #[must_use]
    pub fn new(
        candidate_ref: impl Into<String>,
        work_run_id: impl Into<String>,
        repository_id: impl Into<String>,
    ) -> Self {
        Self {
            candidate_ref: candidate_ref.into(),
            work_run_id: work_run_id.into(),
            repository_id: repository_id.into(),
            base_commit_id: None,
            base_tree_id: None,
            candidate_commit_id: None,
            candidate_tree_id: None,
            diff_sha256: None,
            content_digest: None,
            created_at_millis: None,
        }
    }

    /// Records the base commit and tree the candidate is measured against.
    #[must_use]
    pub fn with_base(mut self, commit_id: impl Into<String>, tree_id: impl Into<String>) -> Self {
        self.base_commit_id = Some(commit_id.into());
        self.base_tree_id = Some(tree_id.into());
        self
    }

    /// Records the exact candidate commit and tree under verification.
    #[must_use]
    pub fn with_candidate(
        mut self,
        commit_id: impl Into<String>,
        tree_id: impl Into<String>,
    ) -> Self {
        self.candidate_commit_id = Some(commit_id.into());
        self.candidate_tree_id = Some(tree_id.into());
        self
    }

    /// Records the fingerprint of the change between base and candidate.
    #[must_use]
    pub fn with_diff_sha256(mut self, digest: impl Into<String>) -> Self {
        self.diff_sha256 = Some(digest.into());
        self
    }

    /// Records the fingerprint of the canonical manifest bytes.
    #[must_use]
    pub fn with_content_digest(mut self, digest: impl Into<String>) -> Self {
        self.content_digest = Some(digest.into());
        self
    }

    /// Records the creation instant. Must precede every bound verification run.
    #[must_use]
    pub fn with_created_at_millis(mut self, millis: u64) -> Self {
        self.created_at_millis = Some(millis);
        self
    }

    /// Builds the sealed Snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::Incomplete`] when any required identity field
    /// was not supplied or a generated identifier is malformed.
    pub fn build(self) -> Result<Snapshot, SnapshotError> {
        let (
            Some(base_commit_id),
            Some(base_tree_id),
            Some(candidate_commit_id),
            Some(candidate_tree_id),
            Some(diff_sha256),
            Some(content_digest),
            Some(created_at_millis),
        ) = (
            self.base_commit_id,
            self.base_tree_id,
            self.candidate_commit_id,
            self.candidate_tree_id,
            self.diff_sha256,
            self.content_digest,
            self.created_at_millis,
        ) else {
            return Err(SnapshotError::Incomplete);
        };
        // The generated identifier newtypes expose no constructors, so the
        // canonical prefixed shape is checked here and the newtype wrapped.
        // `candidate_ref` is stored verbatim: the job carries it as a plain
        // string and nothing produces a `cnd_` id to demand here.
        if self.candidate_ref.is_empty()
            || !is_canonical_prefixed_id(&self.work_run_id, "wrn_")
            || !is_canonical_prefixed_id(&self.repository_id, "rep_")
        {
            return Err(SnapshotError::Incomplete);
        }
        let candidate_ref = self.candidate_ref;
        let work_run_id = WorkRunId(self.work_run_id);
        let repository_id = RepositoryId(self.repository_id);
        let snapshot_id = allocate_snapshot_id(
            &candidate_ref,
            &work_run_id.0,
            &repository_id.0,
            created_at_millis,
        );
        let mut snapshot = Snapshot {
            snapshot_id,
            candidate_ref,
            work_run_id,
            repository_id,
            base_commit_id,
            base_tree_id,
            candidate_commit_id,
            candidate_tree_id,
            diff_sha256,
            content_digest,
            created_at_millis,
            validation_seal: String::new(),
        };
        snapshot.validation_seal = seal_fields(&snapshot);
        Ok(snapshot)
    }
}

/// Mints one fresh `snap_` identity in the canonical identifier encoding
/// (`<prefix>_` + 26-char uppercase Crockford base32).
///
/// Every build allocates its own identifier, so rebuilding the same code
/// identity twice yields two snapshots that share a seal but never an id.
fn allocate_snapshot_id(
    candidate_ref: &str,
    work_run_id: &str,
    repository_id: &str,
    created_at_millis: u64,
) -> SnapshotId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_nanos()).unwrap_or(u64::MAX));
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.snapshot.id.v1\0");
    for field in [
        &nanos.to_be_bytes()[..],
        &counter.to_be_bytes()[..],
        &std::process::id().to_be_bytes()[..],
        &created_at_millis.to_be_bytes()[..],
        candidate_ref.as_bytes(),
        work_run_id.as_bytes(),
        repository_id.as_bytes(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    let digest = hasher.finalize();
    let mut value = u128::from_be_bytes(digest[..16].try_into().unwrap_or([0; 16]));
    let mut suffix = [b'0'; 26];
    for slot in suffix.iter_mut().rev() {
        *slot = ALPHABET[usize::try_from(value & 31).unwrap_or(0)];
        value >>= 5;
    }
    SnapshotId(format!("snap_{}", String::from_utf8_lossy(&suffix)))
}
