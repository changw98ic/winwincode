// SPDX-License-Identifier: Apache-2.0

//! Canonical planning, durable-before-write mutation, and exact rollback for
//! one `ChangeBatch`.
//!
//! This crate turns a generated proposal event into one bounded, deterministic
//! operation plan. Its deep mutation module captures bounded preimages, requires
//! a durable journal acknowledgement, applies through a no-follow filesystem,
//! verifies the exact delta, and rolls failures back with content-digest CAS.

mod mutation;

pub use mutation::{
    AppliedMutationReport, CapturedFile, CapturedWorkspace, ChangeBatchExecutionError,
    ChangeBatchFileSystemPort, ChangeBatchMutationOutcome, ChangeBatchMutationStatus,
    ExecutionJournalError, ExecutionJournalPort, FilePreimage, FileSystemMutationError,
    LocalNoFollowFileSystem, MutationFuture, PreimageJournalValidationError,
    PreparedPreimageJournalRecord, RestoreStep, execute_prepared_change_batch,
    rebuild_preimage_journal_record, recover_prepared_change_batch,
    validate_preimage_journal_record,
};

use std::{
    collections::{BTreeSet, HashSet},
    fmt,
    path::{Component, Path},
};

use codex_apply_patch::{Hunk, parse_patch};
use sha2::{Digest as _, Sha256};
use winwincode_domain::Sha256Digest;
use winwincode_execution_port::{
    change_batch_identity::validate_change_batch_identity_derivation,
    generated::{
        AppliedFileOperation, AppliedFileSummary, ChangeBatchProposalDisposition,
        ChangeBatchProposalEvent,
    },
};

/// Canonical maximum encoded patch size: 512 KiB of UTF-8 bytes.
pub const MAX_PATCH_BYTES: usize = 524_288;
/// Canonical maximum number of unique source and destination paths.
pub const MAX_FILES: usize = 20;
/// Canonical maximum number of effective patch hunks.
pub const MAX_HUNKS: usize = 100;
/// Canonical upper bound for all rollback preimages captured before mutation.
pub const MAX_PREIMAGE_BYTES: u64 = 67_108_864;

const MAX_PATH_BYTES: usize = 4096;
const PLAN_DIGEST_DOMAIN: &[u8] = b"winwincode.change-batch-plan.v1\0";
const DELTA_DIGEST_DOMAIN: &[u8] = b"winwincode.change-batch-delta.v1\0";

/// Runtime-selectable resource policy bounded by the canonical hard limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChangeBatchPolicy {
    max_preimage_bytes: u64,
}

impl ChangeBatchPolicy {
    /// Builds a stricter preimage budget without permitting the canonical hard
    /// limit to be raised.
    ///
    /// # Errors
    ///
    /// Rejects zero or a value above [`MAX_PREIMAGE_BYTES`].
    pub const fn with_max_preimage_bytes(
        max_preimage_bytes: u64,
    ) -> Result<Self, ChangeBatchPlanError> {
        if max_preimage_bytes == 0 || max_preimage_bytes > MAX_PREIMAGE_BYTES {
            return Err(ChangeBatchPlanError::InvalidPolicy);
        }
        Ok(Self { max_preimage_bytes })
    }

    /// Maximum aggregate source bytes the execution adapter may retain for
    /// rollback. It must be checked before the first workspace write.
    #[must_use]
    pub const fn max_preimage_bytes(self) -> u64 {
        self.max_preimage_bytes
    }
}

impl Default for ChangeBatchPolicy {
    fn default() -> Self {
        Self {
            max_preimage_bytes: MAX_PREIMAGE_BYTES,
        }
    }
}

/// One normalized file operation. Upstream parser types never cross this
/// boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlannedFileChange {
    Add {
        path: String,
    },
    Update {
        path: String,
        chunk_count: usize,
    },
    Delete {
        path: String,
    },
    Move {
        source_path: String,
        destination_path: String,
        chunk_count: usize,
    },
}

impl PlannedFileChange {
    /// Primary source or destination used for canonical ordering.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Add { path } | Self::Update { path, .. } | Self::Delete { path } => path,
            Self::Move { source_path, .. } => source_path,
        }
    }

    /// Number of effective hunks charged to the canonical hunk budget.
    #[must_use]
    pub const fn hunk_count(&self) -> usize {
        match self {
            Self::Add { .. } | Self::Delete { .. } => 1,
            Self::Update { chunk_count, .. } | Self::Move { chunk_count, .. } => *chunk_count,
        }
    }
}

/// Required path state that a no-follow filesystem adapter must prove before
/// mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum PreflightPathState {
    /// The path and final leaf must not exist; every existing ancestor must be
    /// a real directory rather than a symbolic link.
    Absent,
    /// The leaf must be one regular UTF-8 text file, with no symbolic link in
    /// any path component. Its bytes and mode contribute to the preimage budget.
    RegularUtf8File,
}

/// One sorted no-follow path requirement for the execution adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreflightPathRequirement {
    path: String,
    state: PreflightPathState,
}

impl PreflightPathRequirement {
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub const fn state(&self) -> PreflightPathState {
        self.state
    }
}

/// Canonical operation plan accepted by the future mutation layer.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedChangeBatchPlan {
    event: ChangeBatchProposalEvent,
    operations: Vec<PlannedFileChange>,
    preflight: Vec<PreflightPathRequirement>,
    touched_paths: Vec<String>,
    hunk_count: usize,
    patch_bytes: usize,
    max_preimage_bytes: u64,
    plan_digest: Sha256Digest,
}

impl PreparedChangeBatchPlan {
    #[must_use]
    pub const fn event(&self) -> &ChangeBatchProposalEvent {
        &self.event
    }

    #[must_use]
    pub fn operations(&self) -> &[PlannedFileChange] {
        &self.operations
    }

    #[must_use]
    pub fn preflight_requirements(&self) -> &[PreflightPathRequirement] {
        &self.preflight
    }

    #[must_use]
    pub fn touched_paths(&self) -> &[String] {
        &self.touched_paths
    }

    #[must_use]
    pub const fn hunk_count(&self) -> usize {
        self.hunk_count
    }

    #[must_use]
    pub const fn patch_bytes(&self) -> usize {
        self.patch_bytes
    }

    #[must_use]
    pub const fn max_preimage_bytes(&self) -> u64 {
        self.max_preimage_bytes
    }

    #[must_use]
    pub const fn plan_digest(&self) -> &Sha256Digest {
        &self.plan_digest
    }
}

/// Validates and prepares one proposal without touching a workspace.
///
/// # Errors
///
/// Rejects a malformed generated contract, changed identity or patch digest,
/// parser failure, resource overflow, non-portable path, or conflicting path
/// graph.
pub fn prepare_change_batch(
    event: &ChangeBatchProposalEvent,
    policy: ChangeBatchPolicy,
) -> Result<PreparedChangeBatchPlan, ChangeBatchPlanError> {
    let input_patch_bytes = event.proposal.patch.len();
    if input_patch_bytes > MAX_PATCH_BYTES {
        return Err(ChangeBatchPlanError::PatchTooLarge {
            actual: input_patch_bytes,
            maximum: MAX_PATCH_BYTES,
        });
    }
    let event = strict_contract_round_trip(event)?;
    validate_change_batch_identity_derivation(&event.identity)
        .map_err(|_| ChangeBatchPlanError::InvalidIdentity)?;
    let patch_bytes = event.proposal.patch.len();
    let actual_patch_digest = digest_bytes(event.proposal.patch.as_bytes());
    if actual_patch_digest != event.identity.patch_digest {
        return Err(ChangeBatchPlanError::PatchDigestMismatch);
    }

    let parsed =
        parse_patch(&event.proposal.patch).map_err(|_| ChangeBatchPlanError::InvalidPatch)?;
    if parsed.hunks.is_empty() {
        return Err(ChangeBatchPlanError::EmptyPatch);
    }

    let parts = prepare_operations(parsed.hunks)?;
    let plan_digest = plan_digest(&event, &parts.operations, policy.max_preimage_bytes);
    Ok(PreparedChangeBatchPlan {
        event,
        operations: parts.operations,
        preflight: parts.preflight,
        touched_paths: parts.touched_paths,
        hunk_count: parts.hunk_count,
        patch_bytes,
        max_preimage_bytes: policy.max_preimage_bytes,
        plan_digest,
    })
}

struct PreparedOperationParts {
    operations: Vec<PlannedFileChange>,
    preflight: Vec<PreflightPathRequirement>,
    touched_paths: Vec<String>,
    hunk_count: usize,
}

fn prepare_operations(hunks: Vec<Hunk>) -> Result<PreparedOperationParts, ChangeBatchPlanError> {
    let mut operations = Vec::with_capacity(hunks.len());
    let mut preflight = Vec::new();
    let mut touched = BTreeSet::new();
    let mut claimed = HashSet::new();
    let mut hunk_count = 0_usize;

    for hunk in hunks {
        let operation = match hunk {
            Hunk::AddFile { path, .. } => {
                let path = portable_path(&path)?;
                claim_path(&mut claimed, &path)?;
                touched.insert(path.clone());
                preflight.push(requirement(&path, PreflightPathState::Absent));
                PlannedFileChange::Add { path }
            }
            Hunk::DeleteFile { path } => {
                let path = portable_path(&path)?;
                claim_path(&mut claimed, &path)?;
                touched.insert(path.clone());
                preflight.push(requirement(&path, PreflightPathState::RegularUtf8File));
                PlannedFileChange::Delete { path }
            }
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let source_path = portable_path(&path)?;
                claim_path(&mut claimed, &source_path)?;
                touched.insert(source_path.clone());
                preflight.push(requirement(
                    &source_path,
                    PreflightPathState::RegularUtf8File,
                ));
                let chunk_count = chunks.len().max(1);
                if let Some(destination) = move_path {
                    let destination_path = portable_path(&destination)?;
                    claim_path(&mut claimed, &destination_path)?;
                    touched.insert(destination_path.clone());
                    preflight.push(requirement(&destination_path, PreflightPathState::Absent));
                    PlannedFileChange::Move {
                        source_path,
                        destination_path,
                        chunk_count,
                    }
                } else {
                    PlannedFileChange::Update {
                        path: source_path,
                        chunk_count,
                    }
                }
            }
        };
        hunk_count = hunk_count.checked_add(operation.hunk_count()).ok_or(
            ChangeBatchPlanError::TooManyHunks {
                actual: usize::MAX,
                maximum: MAX_HUNKS,
            },
        )?;
        if hunk_count > MAX_HUNKS {
            return Err(ChangeBatchPlanError::TooManyHunks {
                actual: hunk_count,
                maximum: MAX_HUNKS,
            });
        }
        if touched.len() > MAX_FILES {
            return Err(ChangeBatchPlanError::TooManyFiles {
                actual: touched.len(),
                maximum: MAX_FILES,
            });
        }
        operations.push(operation);
    }

    operations.sort_by(compare_operations);
    preflight.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(PreparedOperationParts {
        operations,
        preflight,
        touched_paths: touched.into_iter().collect(),
        hunk_count,
    })
}

/// Sorts and validates secret-safe applied-file summaries.
///
/// # Errors
///
/// Rejects more than 20 touched paths, conflicting source/destination paths,
/// non-portable paths, non-canonical digests or modes, negative byte counts,
/// and operation/optional-field combinations that cannot represent an exact
/// Add, Update, Delete, or Move.
pub fn canonical_applied_file_summaries(
    summaries: &[AppliedFileSummary],
) -> Result<Vec<AppliedFileSummary>, AppliedDeltaError> {
    let mut result = summaries.to_vec();
    let mut touched = HashSet::new();
    for summary in &result {
        validate_applied_summary(summary)?;
        claim_delta_path(&mut touched, &summary.path)?;
        if let Some(move_path) = &summary.move_path {
            claim_delta_path(&mut touched, move_path)?;
        }
        if touched.len() > MAX_FILES {
            return Err(AppliedDeltaError::TooManyFiles);
        }
    }
    result.sort_by(compare_summaries);
    Ok(result)
}

/// Derives a stable digest from canonical sorted file summaries using a domain
/// separator, explicit optional-value tags, and unsigned 64-bit length framing.
///
/// # Errors
///
/// Returns the same strict validation failures as
/// [`canonical_applied_file_summaries`].
pub fn derive_delta_digest(
    summaries: &[AppliedFileSummary],
) -> Result<Sha256Digest, AppliedDeltaError> {
    let summaries = canonical_applied_file_summaries(summaries)?;
    let mut hasher = Sha256::new();
    hasher.update(DELTA_DIGEST_DOMAIN);
    frame_u64(&mut hasher, usize_to_u64(summaries.len()));
    for summary in &summaries {
        frame(&mut hasher, summary.path.as_bytes());
        hasher.update([operation_tag(&summary.operation)]);
        frame_optional_text(&mut hasher, summary.move_path.as_deref());
        frame_optional_digest(&mut hasher, summary.before_sha256.as_ref());
        frame_optional_digest(&mut hasher, summary.after_sha256.as_ref());
        frame_u64(&mut hasher, i64_to_u64(summary.bytes_before));
        frame_u64(&mut hasher, i64_to_u64(summary.bytes_after));
        frame_optional_text(&mut hasher, summary.mode_before.as_deref());
        frame_optional_text(&mut hasher, summary.mode_after.as_deref());
    }
    Ok(finish_digest(hasher))
}

fn strict_contract_round_trip(
    event: &ChangeBatchProposalEvent,
) -> Result<ChangeBatchProposalEvent, ChangeBatchPlanError> {
    let bytes = serde_json::to_vec(event).map_err(|_| ChangeBatchPlanError::InvalidContract)?;
    serde_json::from_slice(&bytes).map_err(|_| ChangeBatchPlanError::InvalidContract)
}

fn requirement(path: &str, state: PreflightPathState) -> PreflightPathRequirement {
    PreflightPathRequirement {
        path: path.to_owned(),
        state,
    }
}

fn claim_path(claimed: &mut HashSet<String>, path: &str) -> Result<(), ChangeBatchPlanError> {
    if !claimed.insert(path.to_owned()) {
        return Err(ChangeBatchPlanError::ConflictingPath(path.to_owned()));
    }
    Ok(())
}

fn portable_path(path: &Path) -> Result<String, ChangeBatchPlanError> {
    let text = path.to_str().ok_or(ChangeBatchPlanError::InvalidPath)?;
    if text.is_empty()
        || text.len() > MAX_PATH_BYTES
        || text.contains(['\\', '\0', '<', '>', ':', '"', '|', '?', '*'])
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || path.components().any(|component| {
            let Component::Normal(component) = component else {
                return true;
            };
            let component = component.to_string_lossy();
            component.ends_with([' ', '.']) || windows_reserved_component(&component)
        })
    {
        return Err(ChangeBatchPlanError::InvalidPath);
    }
    let canonical = path
        .components()
        .map(Component::as_os_str)
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if canonical != text {
        return Err(ChangeBatchPlanError::InvalidPath);
    }
    Ok(canonical)
}

fn windows_reserved_component(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

fn compare_operations(left: &PlannedFileChange, right: &PlannedFileChange) -> std::cmp::Ordering {
    operation_sort_key(left).cmp(&operation_sort_key(right))
}

fn operation_sort_key(operation: &PlannedFileChange) -> (&str, u8, &str) {
    match operation {
        PlannedFileChange::Add { path } => (path, 0, ""),
        PlannedFileChange::Update { path, .. } => (path, 1, ""),
        PlannedFileChange::Delete { path } => (path, 2, ""),
        PlannedFileChange::Move {
            source_path,
            destination_path,
            ..
        } => (source_path, 3, destination_path),
    }
}

fn plan_digest(
    event: &ChangeBatchProposalEvent,
    operations: &[PlannedFileChange],
    max_preimage_bytes: u64,
) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(PLAN_DIGEST_DOMAIN);
    frame(&mut hasher, event.identity.batch_id.0.as_bytes());
    frame(&mut hasher, event.identity.patch_digest.0.as_bytes());
    frame_u64(
        &mut hasher,
        u64::try_from(event.proposal.schema_version).unwrap_or(u64::MAX),
    );
    hasher.update([disposition_tag(&event.proposal.disposition)]);
    frame(
        &mut hasher,
        validation_profile_name(&event.proposal.validation_profile).as_bytes(),
    );
    frame_u64(
        &mut hasher,
        usize_to_u64(event.proposal.acceptance_criteria_ids.len()),
    );
    for criterion in &event.proposal.acceptance_criteria_ids {
        frame(&mut hasher, criterion.as_bytes());
    }
    frame_u64(&mut hasher, max_preimage_bytes);
    frame_u64(&mut hasher, usize_to_u64(operations.len()));
    for operation in operations {
        match operation {
            PlannedFileChange::Add { path } => {
                hasher.update([0]);
                frame(&mut hasher, path.as_bytes());
            }
            PlannedFileChange::Update { path, chunk_count } => {
                hasher.update([1]);
                frame(&mut hasher, path.as_bytes());
                frame_u64(&mut hasher, usize_to_u64(*chunk_count));
            }
            PlannedFileChange::Delete { path } => {
                hasher.update([2]);
                frame(&mut hasher, path.as_bytes());
            }
            PlannedFileChange::Move {
                source_path,
                destination_path,
                chunk_count,
            } => {
                hasher.update([3]);
                frame(&mut hasher, source_path.as_bytes());
                frame(&mut hasher, destination_path.as_bytes());
                frame_u64(&mut hasher, usize_to_u64(*chunk_count));
            }
        }
    }
    finish_digest(hasher)
}

const fn validation_profile_name(
    profile: &winwincode_execution_port::generated::ValidationProfileName,
) -> &'static str {
    use winwincode_execution_port::generated::ValidationProfileName;

    match profile {
        ValidationProfileName::Changed => "changed",
        ValidationProfileName::Fast => "fast",
        ValidationProfileName::Affected => "affected",
        ValidationProfileName::Final => "final",
    }
}

fn disposition_tag(disposition: &ChangeBatchProposalDisposition) -> u8 {
    match disposition {
        ChangeBatchProposalDisposition::Final => 0,
        ChangeBatchProposalDisposition::ContinueValue => 1,
        ChangeBatchProposalDisposition::Probe => 2,
    }
}

fn digest_bytes(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn finish_digest(hasher: Sha256) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", hasher.finalize()))
}

fn frame(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(usize_to_u64(bytes.len()).to_be_bytes());
    hasher.update(bytes);
}

fn frame_u64(hasher: &mut Sha256, value: u64) {
    hasher.update(value.to_be_bytes());
}

fn frame_optional_text(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        None => hasher.update([0]),
        Some(value) => {
            hasher.update([1]);
            frame(hasher, value.as_bytes());
        }
    }
}

fn frame_optional_digest(hasher: &mut Sha256, value: Option<&Sha256Digest>) {
    frame_optional_text(hasher, value.map(|digest| digest.0.as_str()));
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn claim_delta_path(touched: &mut HashSet<String>, path: &str) -> Result<(), AppliedDeltaError> {
    if !touched.insert(path.to_owned()) {
        return Err(AppliedDeltaError::ConflictingPath);
    }
    Ok(())
}

fn validate_applied_summary(summary: &AppliedFileSummary) -> Result<(), AppliedDeltaError> {
    portable_path(Path::new(&summary.path)).map_err(|_| AppliedDeltaError::InvalidPath)?;
    if let Some(move_path) = &summary.move_path {
        portable_path(Path::new(move_path)).map_err(|_| AppliedDeltaError::InvalidPath)?;
    }
    if summary.bytes_before < 0
        || summary.bytes_after < 0
        || !summary.before_sha256.as_ref().is_none_or(canonical_digest)
        || !summary.after_sha256.as_ref().is_none_or(canonical_digest)
        || !summary.mode_before.as_deref().is_none_or(canonical_mode)
        || !summary.mode_after.as_deref().is_none_or(canonical_mode)
    {
        return Err(AppliedDeltaError::InvalidSummary);
    }
    let valid_shape = match summary.operation {
        AppliedFileOperation::Create => {
            summary.move_path.is_none()
                && summary.before_sha256.is_none()
                && summary.after_sha256.is_some()
                && summary.bytes_before == 0
                && summary.mode_before.is_none()
                && summary.mode_after.is_some()
        }
        AppliedFileOperation::Update => {
            summary.move_path.is_none()
                && summary.before_sha256.is_some()
                && summary.after_sha256.is_some()
                && summary.mode_before.is_some()
                && summary.mode_after.is_some()
        }
        AppliedFileOperation::Delete => {
            summary.move_path.is_none()
                && summary.before_sha256.is_some()
                && summary.after_sha256.is_none()
                && summary.bytes_after == 0
                && summary.mode_before.is_some()
                && summary.mode_after.is_none()
        }
        AppliedFileOperation::MoveValue => {
            summary.move_path.is_some()
                && summary.before_sha256.is_some()
                && summary.after_sha256.is_some()
                && summary.mode_before.is_some()
                && summary.mode_after.is_some()
        }
    };
    if !valid_shape {
        return Err(AppliedDeltaError::InvalidSummary);
    }
    Ok(())
}

fn canonical_digest(digest: &Sha256Digest) -> bool {
    digest.0.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn canonical_mode(mode: &str) -> bool {
    matches!(mode.len(), 3 | 4) && mode.bytes().all(|byte| matches!(byte, b'0'..=b'7'))
}

fn compare_summaries(left: &AppliedFileSummary, right: &AppliedFileSummary) -> std::cmp::Ordering {
    (
        left.path.as_str(),
        operation_tag(&left.operation),
        left.move_path.as_deref().unwrap_or(""),
    )
        .cmp(&(
            right.path.as_str(),
            operation_tag(&right.operation),
            right.move_path.as_deref().unwrap_or(""),
        ))
}

fn operation_tag(operation: &AppliedFileOperation) -> u8 {
    match operation {
        AppliedFileOperation::Create => 0,
        AppliedFileOperation::Update => 1,
        AppliedFileOperation::Delete => 2,
        AppliedFileOperation::MoveValue => 3,
    }
}

/// Stable planning failures. Parser internals and source text are intentionally
/// absent from errors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChangeBatchPlanError {
    InvalidPolicy,
    InvalidContract,
    InvalidIdentity,
    PatchDigestMismatch,
    PatchTooLarge { actual: usize, maximum: usize },
    InvalidPatch,
    EmptyPatch,
    TooManyFiles { actual: usize, maximum: usize },
    TooManyHunks { actual: usize, maximum: usize },
    InvalidPath,
    ConflictingPath(String),
}

impl fmt::Display for ChangeBatchPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPolicy => "ChangeBatch resource policy is invalid",
            Self::InvalidContract => "ChangeBatch event does not satisfy the canonical contract",
            Self::InvalidIdentity => "ChangeBatch identity derivation is invalid",
            Self::PatchDigestMismatch => "ChangeBatch patch digest does not match its bytes",
            Self::PatchTooLarge { .. } => "ChangeBatch patch exceeds its byte limit",
            Self::InvalidPatch => "ChangeBatch patch syntax is invalid",
            Self::EmptyPatch => "ChangeBatch patch has no operations",
            Self::TooManyFiles { .. } => "ChangeBatch touches too many paths",
            Self::TooManyHunks { .. } => "ChangeBatch contains too many hunks",
            Self::InvalidPath => "ChangeBatch contains a non-portable path",
            Self::ConflictingPath(_) => "ChangeBatch path graph contains a conflict",
        })
    }
}

impl std::error::Error for ChangeBatchPlanError {}

/// Stable failures for exact applied-delta construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppliedDeltaError {
    InvalidPath,
    InvalidSummary,
    ConflictingPath,
    TooManyFiles,
}

impl fmt::Display for AppliedDeltaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPath => "applied file summary contains a non-portable path",
            Self::InvalidSummary => "applied file summary is not exact",
            Self::ConflictingPath => "applied file summaries contain a path conflict",
            Self::TooManyFiles => "applied file summaries touch too many paths",
        })
    }
}

impl std::error::Error for AppliedDeltaError {}
