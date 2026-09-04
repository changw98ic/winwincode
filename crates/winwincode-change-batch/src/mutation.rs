// SPDX-License-Identifier: Apache-2.0

//! Durable-before-write workspace mutation and exact rollback.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::File,
    future::Future,
    io::{self, Read as _, Seek as _, SeekFrom, Write as _},
    os::fd::OwnedFd,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use codex_apply_patch::{
    AppliedPatchDelta, AppliedPatchFileChange, ApplyPatchArgs, ApplyPatchFileChange,
    ApplyPatchFileUpdateMode, ApplyPatchOptions, MaybeApplyPatchVerified, apply_patch_with_options,
    parse_patch, verify_apply_patch_args_with_mode,
};
use codex_exec_server::{
    CopyOptions, CreateDirectoryOptions, ExecutorFileSystem, ExecutorFileSystemFuture,
    FileMetadata, FileSystemReadStream, FileSystemSandboxContext, GetMetadataOptions,
    ReadDirectoryEntry, ReadFileOptions, RemoveOptions, WalkOptions, WalkOutcome, WriteFileOptions,
};
use codex_utils_path_uri::PathUri;
use rustix::fs::{Mode, OFlags, fstat, open, openat, unlinkat};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{ChangeBatchId, Sha256Digest};
use winwincode_execution_port::generated::{AppliedFileOperation, AppliedFileSummary};

use crate::{
    PlannedFileChange, PreflightPathState, PreparedChangeBatchPlan,
    canonical_applied_file_summaries, canonical_digest, derive_delta_digest,
};

const ADDED_FILE_MODE: u32 = 0o644;
const EXECUTABLE_FILE_MODE: u32 = 0o755;
const JOURNAL_DOMAIN: &[u8] = b"winwincode.change-batch-preimages.v1\0";

#[doc(hidden)]
pub type MutationFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One complete preimage captured before the first workspace mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePreimage {
    path: String,
    bytes: Option<Vec<u8>>,
    digest: Option<Sha256Digest>,
    mode: Option<String>,
    expected_after_digest: Option<Sha256Digest>,
    expected_after_mode: Option<String>,
}

impl FilePreimage {
    /// Portable workspace-relative path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Exact bytes, or `None` when the path was absent.
    #[must_use]
    pub fn bytes(&self) -> Option<&[u8]> {
        self.bytes.as_deref()
    }

    /// Digest of the exact bytes, or `None` when absent.
    #[must_use]
    pub const fn digest(&self) -> Option<&Sha256Digest> {
        self.digest.as_ref()
    }

    /// Canonical permission bits (`0644` or `0755`), or `None` when absent.
    #[must_use]
    pub fn mode(&self) -> Option<&str> {
        self.mode.as_deref()
    }

    /// Expected post-apply digest, or `None` when the path must be absent.
    #[must_use]
    pub const fn expected_after_digest(&self) -> Option<&Sha256Digest> {
        self.expected_after_digest.as_ref()
    }

    /// Expected post-apply mode, or `None` when the path must be absent.
    #[must_use]
    pub fn expected_after_mode(&self) -> Option<&str> {
        self.expected_after_mode.as_deref()
    }

    /// Reconstructs one persisted entry. The complete record must still pass
    /// [`rebuild_preimage_journal_record`].
    #[must_use]
    pub fn from_persisted(
        path: String,
        bytes: Option<Vec<u8>>,
        digest: Option<Sha256Digest>,
        mode: Option<String>,
        expected_after_digest: Option<Sha256Digest>,
        expected_after_mode: Option<String>,
    ) -> Self {
        Self {
            path,
            bytes,
            digest,
            mode,
            expected_after_digest,
            expected_after_mode,
        }
    }
}

/// Immutable record that must be durably stored and fsynced before first write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedPreimageJournalRecord {
    batch_id: ChangeBatchId,
    plan_digest: Sha256Digest,
    preimage_digest: Sha256Digest,
    total_preimage_bytes: u64,
    files: Vec<FilePreimage>,
}

impl PreparedPreimageJournalRecord {
    #[must_use]
    pub const fn batch_id(&self) -> &ChangeBatchId {
        &self.batch_id
    }

    #[must_use]
    pub const fn plan_digest(&self) -> &Sha256Digest {
        &self.plan_digest
    }

    #[must_use]
    pub const fn preimage_digest(&self) -> &Sha256Digest {
        &self.preimage_digest
    }

    #[must_use]
    pub const fn total_preimage_bytes(&self) -> u64 {
        self.total_preimage_bytes
    }

    #[must_use]
    pub fn files(&self) -> &[FilePreimage] {
        &self.files
    }
}

/// Stable journal failure. Messages must not contain source bytes or secrets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionJournalError {
    Conflict,
    Capacity,
    Unavailable,
    Corrupt,
}

impl fmt::Display for ExecutionJournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Conflict => "preimage journal already contains conflicting data",
            Self::Capacity => "preimage journal capacity was exceeded",
            Self::Unavailable => "preimage journal is unavailable",
            Self::Corrupt => "preimage journal is corrupt",
        })
    }
}

impl std::error::Error for ExecutionJournalError {}

/// Durable seam owned by the execution layer.
///
/// Returning `Ok(())` promises that the record and its file bytes reached
/// durable storage and that both file data and containing directory metadata
/// were fsynced. The mutation module never writes before this call succeeds.
pub trait ExecutionJournalPort {
    /// # Errors
    ///
    /// Returns a stable storage failure only when durability was not proven.
    fn persist_preimages_and_sync(
        &mut self,
        record: &PreparedPreimageJournalRecord,
    ) -> Result<(), ExecutionJournalError>;
}

/// Filesystem seam used by the deep mutation module.
///
/// The production adapter is [`LocalNoFollowFileSystem`]. Alternative adapters
/// are useful for deterministic fault injection. Upstream parser and filesystem
/// types remain private to this crate.
pub trait ChangeBatchFileSystemPort: Send + Sync {
    #[doc(hidden)]
    fn capture<'a>(
        &'a self,
        root: &'a Path,
        paths: &'a [String],
        byte_limit: u64,
    ) -> MutationFuture<'a, Result<CapturedWorkspace, FileSystemMutationError>>;

    #[doc(hidden)]
    fn apply<'a>(
        &'a self,
        root: &'a Path,
        plan: &'a PreparedChangeBatchPlan,
        preimages: &'a CapturedWorkspace,
    ) -> MutationFuture<'a, Result<AppliedMutationReport, FileSystemMutationError>>;

    #[doc(hidden)]
    fn restore_if_matches<'a>(
        &'a self,
        root: &'a Path,
        path: &'a str,
        expected: &'a CapturedFile,
        before: &'a CapturedFile,
    ) -> MutationFuture<'a, Result<RestoreStep, FileSystemMutationError>>;
}

/// Production local adapter. Every path component is opened without following
/// symbolic links.
#[derive(Clone)]
pub struct LocalNoFollowFileSystem {
    executor: Arc<dyn ExecutorFileSystem>,
}

impl Default for LocalNoFollowFileSystem {
    fn default() -> Self {
        Self {
            executor: Arc::clone(&codex_exec_server::LOCAL_FS),
        }
    }
}

impl LocalNoFollowFileSystem {
    #[cfg(test)]
    fn with_executor(executor: Arc<dyn ExecutorFileSystem>) -> Self {
        Self { executor }
    }
}

/// Final classification of one attempted mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeBatchMutationStatus {
    PreMutation,
    Applied,
    ExactRolledBack,
    PartiallyApplied,
    StateUncertain,
}

/// Exact observable result. `files` and `delta_digest` are present only when
/// the final workspace delta can be represented exactly.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeBatchMutationOutcome {
    status: ChangeBatchMutationStatus,
    files: Vec<AppliedFileSummary>,
    delta_digest: Option<Sha256Digest>,
}

impl ChangeBatchMutationOutcome {
    #[must_use]
    pub const fn status(&self) -> ChangeBatchMutationStatus {
        self.status
    }

    #[must_use]
    pub fn files(&self) -> &[AppliedFileSummary] {
        &self.files
    }

    #[must_use]
    pub const fn delta_digest(&self) -> Option<&Sha256Digest> {
        self.delta_digest.as_ref()
    }
}

/// Failure before mutation begins. Failures after the durable journal step are
/// returned as a classified [`ChangeBatchMutationOutcome`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeBatchExecutionError {
    InvalidWorkspaceRoot,
    InvalidPreflightState,
    PreimageLimitExceeded,
    ConcurrentWorkspaceChange,
    Journal(ExecutionJournalError),
    FileSystemUnavailable,
}

impl fmt::Display for ChangeBatchExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidWorkspaceRoot => "workspace root is not a real directory",
            Self::InvalidPreflightState => "workspace does not satisfy the prepared plan",
            Self::PreimageLimitExceeded => "rollback preimages exceed the configured limit",
            Self::ConcurrentWorkspaceChange => "workspace changed after preflight",
            Self::Journal(_) => "rollback preimages were not durably recorded",
            Self::FileSystemUnavailable => "workspace filesystem is unavailable",
        })
    }
}

impl std::error::Error for ChangeBatchExecutionError {}

/// Executes a prepared plan after durable preimage capture.
///
/// # Errors
///
/// Returns only failures proven to occur before the first mutation. Once apply
/// begins, the function always attempts reverse rollback and returns an exact
/// final-state classification.
pub async fn execute_prepared_change_batch<F: ChangeBatchFileSystemPort>(
    plan: &PreparedChangeBatchPlan,
    workspace_root: &Path,
    journal: &mut impl ExecutionJournalPort,
    file_system: &F,
) -> Result<ChangeBatchMutationOutcome, ChangeBatchExecutionError> {
    let root = trusted_workspace_root(workspace_root)?;
    let before = file_system
        .capture(&root, plan.touched_paths(), plan.max_preimage_bytes())
        .await
        .map_err(map_preflight_fs_error)?;
    validate_preflight(plan, &before)?;
    let expected = expected_mutation_report(plan, &before)
        .await
        .map_err(map_preflight_fs_error)?;
    let record = prepare_journal_record(plan, &before, &expected)?;
    journal
        .persist_preimages_and_sync(&record)
        .map_err(ChangeBatchExecutionError::Journal)?;

    let confirmed = file_system
        .capture(&root, plan.touched_paths(), plan.max_preimage_bytes())
        .await
        .map_err(|_| ChangeBatchExecutionError::ConcurrentWorkspaceChange)?;
    if confirmed != before {
        return Err(ChangeBatchExecutionError::ConcurrentWorkspaceChange);
    }

    let apply_result = file_system.apply(&root, plan, &before).await;
    let after = file_system
        .capture(
            &root,
            plan.touched_paths(),
            plan.max_preimage_bytes()
                .saturating_add(u64::try_from(plan.patch_bytes()).unwrap_or(u64::MAX)),
        )
        .await;

    if let (Ok(report), Ok(after)) = (&apply_result, &after)
        && let Ok(files) = exact_planned_delta(plan, &before, after, report)
    {
        let delta_digest = derive_delta_digest(&files).ok();
        if delta_digest.is_some() {
            return Ok(ChangeBatchMutationOutcome {
                status: ChangeBatchMutationStatus::Applied,
                files,
                delta_digest,
            });
        }
    }

    rollback(plan, &root, file_system, &before, after.ok()).await
}

/// Recovers a durable, possibly interrupted mutation without replaying a
/// workspace write. Current files must each match either the persisted before
/// fingerprint or the canonical expected-after fingerprint before rollback is
/// attempted.
///
/// # Errors
///
/// Rejects a changed persisted manifest, invalid workspace root, unavailable
/// filesystem, or a manifest whose expected state no longer matches the plan.
pub async fn recover_prepared_change_batch<F: ChangeBatchFileSystemPort>(
    plan: &PreparedChangeBatchPlan,
    workspace_root: &Path,
    record: &PreparedPreimageJournalRecord,
    file_system: &F,
) -> Result<ChangeBatchMutationOutcome, ChangeBatchExecutionError> {
    validate_preimage_journal_record(plan, record)
        .map_err(|_| ChangeBatchExecutionError::InvalidPreflightState)?;
    let root = trusted_workspace_root(workspace_root)?;
    let before = workspace_from_preimages(record.files())
        .map_err(|_| ChangeBatchExecutionError::InvalidPreflightState)?;
    let expected = expected_mutation_report(plan, &before)
        .await
        .map_err(map_preflight_fs_error)?;
    if expected.after_digests != expected_digests_from_record(record) {
        return Err(ChangeBatchExecutionError::InvalidPreflightState);
    }
    let current = file_system
        .capture(
            &root,
            plan.touched_paths(),
            plan.max_preimage_bytes()
                .saturating_add(u64::try_from(plan.patch_bytes()).unwrap_or(u64::MAX)),
        )
        .await
        .map_err(|_| ChangeBatchExecutionError::FileSystemUnavailable)?;
    if current == before {
        return Ok(ChangeBatchMutationOutcome {
            status: ChangeBatchMutationStatus::PreMutation,
            files: Vec::new(),
            delta_digest: None,
        });
    }
    if let Ok(files) = exact_planned_delta(plan, &before, &current, &expected) {
        let delta_digest = derive_delta_digest(&files)
            .map_err(|_| ChangeBatchExecutionError::InvalidPreflightState)?;
        return Ok(ChangeBatchMutationOutcome {
            status: ChangeBatchMutationStatus::Applied,
            files,
            delta_digest: Some(delta_digest),
        });
    }
    recover_rollback(plan, record, &root, file_system, &before, current).await
}

async fn recover_rollback<F: ChangeBatchFileSystemPort>(
    plan: &PreparedChangeBatchPlan,
    record: &PreparedPreimageJournalRecord,
    root: &Path,
    file_system: &F,
    before: &CapturedWorkspace,
    current: CapturedWorkspace,
) -> Result<ChangeBatchMutationOutcome, ChangeBatchExecutionError> {
    let mut uncertain = false;
    for path in rollback_order(plan) {
        let (Some(observed), Some(original), Some(entry)) = (
            current.files.get(path),
            before.files.get(path),
            record.files.iter().find(|entry| entry.path == path),
        ) else {
            uncertain = true;
            continue;
        };
        if observed == original {
            continue;
        }
        if !matches_expected_entry(observed, entry) {
            uncertain = true;
            continue;
        }
        match file_system
            .restore_if_matches(root, path, observed, original)
            .await
        {
            Ok(RestoreStep::Restored | RestoreStep::AlreadyOriginal) => {}
            Ok(RestoreStep::CasMismatch) | Err(_) => uncertain = true,
        }
    }
    let final_state = file_system
        .capture(
            root,
            plan.touched_paths(),
            plan.max_preimage_bytes()
                .saturating_add(u64::try_from(plan.patch_bytes()).unwrap_or(u64::MAX)),
        )
        .await;
    let Ok(final_state) = final_state else {
        return Ok(uncertain_outcome());
    };
    if final_state == *before {
        return Ok(ChangeBatchMutationOutcome {
            status: ChangeBatchMutationStatus::ExactRolledBack,
            files: Vec::new(),
            delta_digest: None,
        });
    }
    if uncertain {
        return Ok(uncertain_outcome());
    }
    if let Ok(files) = residual_delta(plan, before, &final_state)
        && let Ok(delta_digest) = derive_delta_digest(&files)
    {
        return Ok(ChangeBatchMutationOutcome {
            status: ChangeBatchMutationStatus::PartiallyApplied,
            files,
            delta_digest: Some(delta_digest),
        });
    }
    Ok(uncertain_outcome())
}

fn map_preflight_fs_error(error: FileSystemMutationError) -> ChangeBatchExecutionError {
    match error {
        FileSystemMutationError::ByteLimit => ChangeBatchExecutionError::PreimageLimitExceeded,
        FileSystemMutationError::InvalidEntry => ChangeBatchExecutionError::InvalidPreflightState,
        FileSystemMutationError::Unavailable => ChangeBatchExecutionError::FileSystemUnavailable,
        FileSystemMutationError::Apply | FileSystemMutationError::CasMismatch => {
            ChangeBatchExecutionError::InvalidPreflightState
        }
    }
}

fn trusted_workspace_root(root: &Path) -> Result<PathBuf, ChangeBatchExecutionError> {
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|_| ChangeBatchExecutionError::InvalidWorkspaceRoot)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ChangeBatchExecutionError::InvalidWorkspaceRoot);
    }
    std::fs::canonicalize(root).map_err(|_| ChangeBatchExecutionError::InvalidWorkspaceRoot)
}

fn validate_preflight(
    plan: &PreparedChangeBatchPlan,
    workspace: &CapturedWorkspace,
) -> Result<(), ChangeBatchExecutionError> {
    for requirement in plan.preflight_requirements() {
        let Some(state) = workspace.files.get(requirement.path()) else {
            return Err(ChangeBatchExecutionError::InvalidPreflightState);
        };
        let valid = matches!(
            (requirement.state(), state),
            (PreflightPathState::Absent, CapturedFile::Absent)
                | (
                    PreflightPathState::RegularUtf8File,
                    CapturedFile::Regular { .. }
                )
        );
        if !valid {
            return Err(ChangeBatchExecutionError::InvalidPreflightState);
        }
    }
    Ok(())
}

fn prepare_journal_record(
    plan: &PreparedChangeBatchPlan,
    workspace: &CapturedWorkspace,
    expected: &AppliedMutationReport,
) -> Result<PreparedPreimageJournalRecord, ChangeBatchExecutionError> {
    if workspace.total_bytes > plan.max_preimage_bytes() {
        return Err(ChangeBatchExecutionError::PreimageLimitExceeded);
    }
    let files = workspace
        .files
        .iter()
        .map(|(path, state)| {
            let expected_after_digest = expected.after_digests.get(path).cloned();
            let expected_after_mode =
                expected_mode_for_path(plan, workspace, path).map(format_mode);
            match state {
                CapturedFile::Absent => FilePreimage {
                    path: path.clone(),
                    bytes: None,
                    digest: None,
                    mode: None,
                    expected_after_digest,
                    expected_after_mode,
                },
                CapturedFile::Regular {
                    bytes,
                    digest,
                    mode,
                    ..
                } => FilePreimage {
                    path: path.clone(),
                    bytes: Some(bytes.clone()),
                    digest: Some(digest.clone()),
                    mode: Some(format_mode(*mode)),
                    expected_after_digest,
                    expected_after_mode,
                },
            }
        })
        .collect::<Vec<_>>();
    let preimage_digest = preimage_digest(plan, &files);
    let record = PreparedPreimageJournalRecord {
        batch_id: plan.event().identity.batch_id.clone(),
        plan_digest: plan.plan_digest().clone(),
        preimage_digest,
        total_preimage_bytes: workspace.total_bytes,
        files,
    };
    validate_preimage_journal_record(plan, &record)
        .map_err(|_| ChangeBatchExecutionError::InvalidPreflightState)?;
    Ok(record)
}

fn preimage_digest(plan: &PreparedChangeBatchPlan, files: &[FilePreimage]) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(JOURNAL_DOMAIN);
    frame(&mut hasher, plan.event().identity.batch_id.0.as_bytes());
    frame(&mut hasher, plan.plan_digest().0.as_bytes());
    frame_u64(&mut hasher, u64::try_from(files.len()).unwrap_or(u64::MAX));
    for file in files {
        frame(&mut hasher, file.path.as_bytes());
        match &file.bytes {
            None => hasher.update([0]),
            Some(bytes) => {
                hasher.update([1]);
                frame(&mut hasher, bytes);
                frame(&mut hasher, file.mode.as_deref().unwrap_or("").as_bytes());
            }
        }
        frame_optional_digest(&mut hasher, file.expected_after_digest.as_ref());
        frame_optional_text(&mut hasher, file.expected_after_mode.as_deref());
    }
    Sha256Digest(format!("sha256:{:x}", hasher.finalize()))
}

/// Stable validation failure for a persisted preimage manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreimageJournalValidationError {
    Identity,
    Plan,
    Paths,
    BeforeState,
    ExpectedState,
    Capacity,
    Digest,
}

impl fmt::Display for PreimageJournalValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Identity => "preimage manifest batch identity changed",
            Self::Plan => "preimage manifest plan digest changed",
            Self::Paths => "preimage manifest paths are not canonical",
            Self::BeforeState => "preimage manifest before-state is invalid",
            Self::ExpectedState => "preimage manifest expected state is invalid",
            Self::Capacity => "preimage manifest exceeds its byte limit",
            Self::Digest => "preimage manifest digest changed",
        })
    }
}

impl std::error::Error for PreimageJournalValidationError {}

/// Validates every persisted manifest field and recomputes the canonical
/// domain-framed digest.
///
/// # Errors
///
/// Rejects changed identity/plan/path order, inconsistent bytes/digests/modes,
/// a changed byte total, capacity overflow, or a changed manifest digest.
pub fn validate_preimage_journal_record(
    plan: &PreparedChangeBatchPlan,
    record: &PreparedPreimageJournalRecord,
) -> Result<(), PreimageJournalValidationError> {
    if record.batch_id != plan.event().identity.batch_id {
        return Err(PreimageJournalValidationError::Identity);
    }
    if record.plan_digest != *plan.plan_digest() {
        return Err(PreimageJournalValidationError::Plan);
    }
    if record.files.len() != plan.touched_paths().len()
        || record
            .files
            .iter()
            .map(|file| file.path.as_str())
            .ne(plan.touched_paths().iter().map(String::as_str))
    {
        return Err(PreimageJournalValidationError::Paths);
    }
    let mut total = 0_u64;
    for file in &record.files {
        match (&file.bytes, &file.digest, &file.mode) {
            (None, None, None) => {}
            (Some(bytes), Some(digest), Some(mode)) => {
                if digest_bytes(bytes) != *digest || !valid_file_mode(mode) {
                    return Err(PreimageJournalValidationError::BeforeState);
                }
                total = total
                    .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
                    .ok_or(PreimageJournalValidationError::Capacity)?;
            }
            _ => return Err(PreimageJournalValidationError::BeforeState),
        }
        match (&file.expected_after_digest, &file.expected_after_mode) {
            (None, None) => {}
            (Some(digest), Some(mode)) if canonical_digest(digest) && valid_file_mode(mode) => {}
            _ => return Err(PreimageJournalValidationError::ExpectedState),
        }
    }
    if total != record.total_preimage_bytes || total > plan.max_preimage_bytes() {
        return Err(PreimageJournalValidationError::Capacity);
    }
    let before = workspace_from_preimages(&record.files)
        .map_err(|_| PreimageJournalValidationError::BeforeState)?;
    for file in &record.files {
        if file.expected_after_mode.as_deref()
            != expected_mode_for_path(plan, &before, &file.path)
                .map(format_mode)
                .as_deref()
        {
            return Err(PreimageJournalValidationError::ExpectedState);
        }
    }
    if record.preimage_digest != preimage_digest(plan, &record.files) {
        return Err(PreimageJournalValidationError::Digest);
    }
    Ok(())
}

/// Reconstructs and validates the exact persisted record without copying the
/// private domain-framing algorithm into a storage adapter.
///
/// # Errors
///
/// Returns the same strict failures as [`validate_preimage_journal_record`].
pub fn rebuild_preimage_journal_record(
    plan: &PreparedChangeBatchPlan,
    persisted_preimage_digest: Sha256Digest,
    persisted_total_preimage_bytes: u64,
    files: Vec<FilePreimage>,
) -> Result<PreparedPreimageJournalRecord, PreimageJournalValidationError> {
    let record = PreparedPreimageJournalRecord {
        batch_id: plan.event().identity.batch_id.clone(),
        plan_digest: plan.plan_digest().clone(),
        preimage_digest: persisted_preimage_digest,
        total_preimage_bytes: persisted_total_preimage_bytes,
        files,
    };
    validate_preimage_journal_record(plan, &record)?;
    Ok(record)
}

fn workspace_from_preimages(
    files: &[FilePreimage],
) -> Result<CapturedWorkspace, PreimageJournalValidationError> {
    let mut states = BTreeMap::new();
    let mut total_bytes = 0_u64;
    for file in files {
        let state = match (&file.bytes, &file.digest, &file.mode) {
            (None, None, None) => CapturedFile::Absent,
            (Some(bytes), Some(digest), Some(mode)) => {
                let mode =
                    parse_file_mode(mode).ok_or(PreimageJournalValidationError::BeforeState)?;
                total_bytes = total_bytes
                    .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
                    .ok_or(PreimageJournalValidationError::Capacity)?;
                CapturedFile::Regular {
                    bytes: bytes.clone(),
                    digest: digest.clone(),
                    mode,
                }
            }
            _ => return Err(PreimageJournalValidationError::BeforeState),
        };
        if states.insert(file.path.clone(), state).is_some() {
            return Err(PreimageJournalValidationError::Paths);
        }
    }
    Ok(CapturedWorkspace {
        files: states,
        total_bytes,
    })
}

fn expected_digests_from_record(
    record: &PreparedPreimageJournalRecord,
) -> BTreeMap<String, Sha256Digest> {
    record
        .files
        .iter()
        .filter_map(|file| {
            file.expected_after_digest
                .clone()
                .map(|digest| (file.path.clone(), digest))
        })
        .collect()
}

fn matches_expected_entry(observed: &CapturedFile, entry: &FilePreimage) -> bool {
    match (
        observed,
        &entry.expected_after_digest,
        entry.expected_after_mode.as_deref(),
    ) {
        (CapturedFile::Absent, None, None) => true,
        (
            CapturedFile::Regular { digest, mode, .. },
            Some(expected_digest),
            Some(expected_mode),
        ) => digest == expected_digest && format_mode(*mode) == expected_mode,
        _ => false,
    }
}

fn expected_mode_for_path(
    plan: &PreparedChangeBatchPlan,
    before: &CapturedWorkspace,
    path: &str,
) -> Option<u32> {
    for operation in plan.operations() {
        match operation {
            PlannedFileChange::Add { path: added } if added == path => {
                return Some(ADDED_FILE_MODE);
            }
            PlannedFileChange::Update { path: updated, .. } if updated == path => {
                return before.files.get(path).and_then(CapturedFile::mode);
            }
            PlannedFileChange::Delete { path: deleted } if deleted == path => return None,
            PlannedFileChange::Move {
                source_path,
                destination_path,
                ..
            } => {
                if source_path == path {
                    return None;
                }
                if destination_path == path {
                    return before.files.get(source_path).and_then(CapturedFile::mode);
                }
            }
            _ => {}
        }
    }
    None
}

fn valid_file_mode(mode: &str) -> bool {
    parse_file_mode(mode).is_some()
}

fn parse_file_mode(mode: &str) -> Option<u32> {
    let parsed = u32::from_str_radix(mode, 8).ok()?;
    matches!(parsed, ADDED_FILE_MODE | EXECUTABLE_FILE_MODE).then_some(parsed)
}

async fn expected_mutation_report(
    plan: &PreparedChangeBatchPlan,
    before: &CapturedWorkspace,
) -> Result<AppliedMutationReport, FileSystemMutationError> {
    let parsed =
        parse_patch(&plan.event().proposal.patch).map_err(|_| FileSystemMutationError::Apply)?;
    let root = Path::new("/winwincode-change-batch-preview");
    let cwd =
        PathUri::from_host_native_path(root).map_err(|_| FileSystemMutationError::Unavailable)?;
    let preview_fs = PreimagePreviewFileSystem {
        root: root.to_path_buf(),
        files: before.files.clone(),
    };
    let verified = verify_apply_patch_args_with_mode(
        ApplyPatchArgs {
            patch: plan.event().proposal.patch.clone(),
            hunks: parsed.hunks,
            workdir: None,
            environment_id: None,
        },
        &cwd,
        ApplyPatchFileUpdateMode::PreserveLineEndings,
        &preview_fs,
        None,
    )
    .await;
    let MaybeApplyPatchVerified::Body(action) = verified else {
        return Err(FileSystemMutationError::Apply);
    };
    let mut operations = BTreeSet::new();
    let mut after_digests = BTreeMap::new();
    for (path, change) in action.changes() {
        let path = relative_uri_path(root, path)?;
        let operation = match change {
            ApplyPatchFileChange::Add { content } => {
                insert_report_digest(&mut after_digests, &path, content)?;
                OperationKey::Add(path)
            }
            ApplyPatchFileChange::Delete { .. } => OperationKey::Delete(path),
            ApplyPatchFileChange::Update {
                move_path,
                new_content,
                ..
            } => {
                if let Some(destination) = move_path {
                    let destination = relative_uri_path(root, destination)?;
                    insert_report_digest(&mut after_digests, &destination, new_content)?;
                    OperationKey::Move(path, destination)
                } else {
                    insert_report_digest(&mut after_digests, &path, new_content)?;
                    OperationKey::Update(path)
                }
            }
        };
        if !operations.insert(operation) {
            return Err(FileSystemMutationError::Apply);
        }
    }
    if operations != expected_operation_keys(plan) {
        return Err(FileSystemMutationError::Apply);
    }
    Ok(AppliedMutationReport {
        exact: true,
        operations,
        before_digests: workspace_digests(before),
        after_digests,
    })
}

struct PreimagePreviewFileSystem {
    root: PathBuf,
    files: BTreeMap<String, CapturedFile>,
}

impl PreimagePreviewFileSystem {
    fn unsupported<T>() -> ExecutorFileSystemFuture<'static, T> {
        Box::pin(async {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "preview operation",
            ))
        })
    }

    fn relative_path(&self, path: &PathUri) -> io::Result<String> {
        let absolute = path.to_abs_path()?;
        absolute
            .as_path()
            .strip_prefix(&self.root)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "preview path escaped"))?
            .to_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "preview path invalid"))
    }
}

impl ExecutorFileSystem for PreimagePreviewFileSystem {
    fn canonicalize<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Self::unsupported()
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        _options: ReadFileOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let relative = self.relative_path(path)?;
            match self.files.get(&relative) {
                Some(CapturedFile::Regular { bytes, .. }) => Ok(bytes.clone()),
                _ => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "preview file absent",
                )),
            }
        })
    }

    fn read_file_stream<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Self::unsupported()
    }

    fn write_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _contents: Vec<u8>,
        _options: WriteFileOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Self::unsupported()
    }

    fn create_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: CreateDirectoryOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Self::unsupported()
    }

    fn get_metadata<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: GetMetadataOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Self::unsupported()
    }

    fn read_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Self::unsupported()
    }

    fn walk<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: WalkOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        Self::unsupported()
    }

    fn remove<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: RemoveOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Self::unsupported()
    }

    fn copy<'a>(
        &'a self,
        _source_path: &'a PathUri,
        _destination_path: &'a PathUri,
        _options: CopyOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Self::unsupported()
    }
}

async fn rollback<F: ChangeBatchFileSystemPort>(
    plan: &PreparedChangeBatchPlan,
    root: &Path,
    file_system: &F,
    before: &CapturedWorkspace,
    after: Option<CapturedWorkspace>,
) -> Result<ChangeBatchMutationOutcome, ChangeBatchExecutionError> {
    let Some(after) = after else {
        return Ok(uncertain_outcome());
    };
    let mut rollback_failed = false;
    let mut cas_mismatch = false;
    for path in rollback_order(plan) {
        let (Some(expected), Some(original)) = (after.files.get(path), before.files.get(path))
        else {
            cas_mismatch = true;
            continue;
        };
        match file_system
            .restore_if_matches(root, path, expected, original)
            .await
        {
            Ok(RestoreStep::Restored | RestoreStep::AlreadyOriginal) => {}
            Ok(RestoreStep::CasMismatch) | Err(FileSystemMutationError::CasMismatch) => {
                cas_mismatch = true;
            }
            Err(_) => rollback_failed = true,
        }
    }

    let final_state = file_system
        .capture(
            root,
            plan.touched_paths(),
            plan.max_preimage_bytes()
                .saturating_add(u64::try_from(plan.patch_bytes()).unwrap_or(u64::MAX)),
        )
        .await;
    let Ok(final_state) = final_state else {
        return Ok(uncertain_outcome());
    };
    if final_state == *before {
        return Ok(ChangeBatchMutationOutcome {
            status: ChangeBatchMutationStatus::ExactRolledBack,
            files: Vec::new(),
            delta_digest: None,
        });
    }
    if cas_mismatch {
        return Ok(uncertain_outcome());
    }
    if let Ok(files) = residual_delta(plan, before, &final_state)
        && let Ok(delta_digest) = derive_delta_digest(&files)
    {
        return Ok(ChangeBatchMutationOutcome {
            status: ChangeBatchMutationStatus::PartiallyApplied,
            files,
            delta_digest: Some(delta_digest),
        });
    }
    let _ = rollback_failed;
    Ok(uncertain_outcome())
}

fn uncertain_outcome() -> ChangeBatchMutationOutcome {
    ChangeBatchMutationOutcome {
        status: ChangeBatchMutationStatus::StateUncertain,
        files: Vec::new(),
        delta_digest: None,
    }
}

fn rollback_order(plan: &PreparedChangeBatchPlan) -> Vec<&str> {
    let mut paths = Vec::with_capacity(plan.touched_paths().len());
    for operation in plan.operations().iter().rev() {
        match operation {
            PlannedFileChange::Add { path }
            | PlannedFileChange::Update { path, .. }
            | PlannedFileChange::Delete { path } => paths.push(path.as_str()),
            PlannedFileChange::Move {
                source_path,
                destination_path,
                ..
            } => {
                paths.push(destination_path);
                paths.push(source_path);
            }
        }
    }
    paths
}

fn exact_planned_delta(
    plan: &PreparedChangeBatchPlan,
    before: &CapturedWorkspace,
    after: &CapturedWorkspace,
    report: &AppliedMutationReport,
) -> Result<Vec<AppliedFileSummary>, ()> {
    if !report.exact
        || report.operations != expected_operation_keys(plan)
        || report.before_digests != workspace_digests(before)
        || report.after_digests != workspace_digests(after)
    {
        return Err(());
    }
    let files = summaries_for_plan(plan, before, after, false)?;
    canonical_applied_file_summaries(&files).map_err(|_| ())
}

fn residual_delta(
    plan: &PreparedChangeBatchPlan,
    before: &CapturedWorkspace,
    after: &CapturedWorkspace,
) -> Result<Vec<AppliedFileSummary>, ()> {
    let files = summaries_for_plan(plan, before, after, true)?;
    canonical_applied_file_summaries(&files).map_err(|_| ())
}

fn summaries_for_plan(
    plan: &PreparedChangeBatchPlan,
    before: &CapturedWorkspace,
    after: &CapturedWorkspace,
    allow_unchanged: bool,
) -> Result<Vec<AppliedFileSummary>, ()> {
    let mut summaries = Vec::new();
    for operation in plan.operations() {
        match operation {
            PlannedFileChange::Add { path } => {
                let (old, new) = states(before, after, path)?;
                if allow_unchanged && old == new {
                    continue;
                }
                let CapturedFile::Absent = old else {
                    return Err(());
                };
                let CapturedFile::Regular { mode, .. } = new else {
                    return Err(());
                };
                if *mode != ADDED_FILE_MODE {
                    return Err(());
                }
                summaries.push(summary(path, AppliedFileOperation::Create, None, old, new)?);
            }
            PlannedFileChange::Update { path, .. } => {
                let (old, new) = states(before, after, path)?;
                if allow_unchanged && old == new {
                    continue;
                }
                let (
                    CapturedFile::Regular { mode: old_mode, .. },
                    CapturedFile::Regular { mode: new_mode, .. },
                ) = (old, new)
                else {
                    return Err(());
                };
                if old_mode != new_mode {
                    return Err(());
                }
                summaries.push(summary(path, AppliedFileOperation::Update, None, old, new)?);
            }
            PlannedFileChange::Delete { path } => {
                let (old, new) = states(before, after, path)?;
                if allow_unchanged && old == new {
                    continue;
                }
                if !matches!(old, CapturedFile::Regular { .. })
                    || !matches!(new, CapturedFile::Absent)
                {
                    return Err(());
                }
                summaries.push(summary(path, AppliedFileOperation::Delete, None, old, new)?);
            }
            PlannedFileChange::Move {
                source_path,
                destination_path,
                ..
            } => {
                let (source_old, source_new) = states(before, after, source_path)?;
                let (destination_old, destination_new) = states(before, after, destination_path)?;
                if allow_unchanged && source_old == source_new && destination_old == destination_new
                {
                    continue;
                }
                let (
                    CapturedFile::Regular { mode: old_mode, .. },
                    CapturedFile::Absent,
                    CapturedFile::Absent,
                    CapturedFile::Regular { mode: new_mode, .. },
                ) = (source_old, source_new, destination_old, destination_new)
                else {
                    return Err(());
                };
                if old_mode != new_mode {
                    return Err(());
                }
                summaries.push(summary(
                    source_path,
                    AppliedFileOperation::MoveValue,
                    Some(destination_path.clone()),
                    source_old,
                    destination_new,
                )?);
            }
        }
    }
    Ok(summaries)
}

fn states<'a>(
    before: &'a CapturedWorkspace,
    after: &'a CapturedWorkspace,
    path: &str,
) -> Result<(&'a CapturedFile, &'a CapturedFile), ()> {
    Ok((
        before.files.get(path).ok_or(())?,
        after.files.get(path).ok_or(())?,
    ))
}

fn summary(
    path: &str,
    operation: AppliedFileOperation,
    move_path: Option<String>,
    before: &CapturedFile,
    after: &CapturedFile,
) -> Result<AppliedFileSummary, ()> {
    Ok(AppliedFileSummary {
        path: path.to_owned(),
        operation,
        move_path,
        before_sha256: before.digest().cloned(),
        after_sha256: after.digest().cloned(),
        bytes_before: i64::try_from(before.byte_len()).map_err(|_| ())?,
        bytes_after: i64::try_from(after.byte_len()).map_err(|_| ())?,
        mode_before: before.mode().map(format_mode),
        mode_after: after.mode().map(format_mode),
    })
}

fn expected_operation_keys(plan: &PreparedChangeBatchPlan) -> BTreeSet<OperationKey> {
    plan.operations()
        .iter()
        .map(|operation| match operation {
            PlannedFileChange::Add { path } => OperationKey::Add(path.clone()),
            PlannedFileChange::Update { path, .. } => OperationKey::Update(path.clone()),
            PlannedFileChange::Delete { path } => OperationKey::Delete(path.clone()),
            PlannedFileChange::Move {
                source_path,
                destination_path,
                ..
            } => OperationKey::Move(source_path.clone(), destination_path.clone()),
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedWorkspace {
    files: BTreeMap<String, CapturedFile>,
    total_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapturedFile {
    Absent,
    Regular {
        bytes: Vec<u8>,
        digest: Sha256Digest,
        mode: u32,
    },
}

impl CapturedFile {
    fn digest(&self) -> Option<&Sha256Digest> {
        match self {
            Self::Absent => None,
            Self::Regular { digest, .. } => Some(digest),
        }
    }

    const fn byte_len(&self) -> u64 {
        match self {
            Self::Absent => 0,
            Self::Regular { bytes, .. } => bytes.len() as u64,
        }
    }

    const fn mode(&self) -> Option<u32> {
        match self {
            Self::Absent => None,
            Self::Regular { mode, .. } => Some(*mode),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum OperationKey {
    Add(String),
    Update(String),
    Delete(String),
    Move(String, String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedMutationReport {
    exact: bool,
    operations: BTreeSet<OperationKey>,
    before_digests: BTreeMap<String, Sha256Digest>,
    after_digests: BTreeMap<String, Sha256Digest>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestoreStep {
    Restored,
    AlreadyOriginal,
    CasMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileSystemMutationError {
    InvalidEntry,
    ByteLimit,
    Unavailable,
    Apply,
    CasMismatch,
}

impl ChangeBatchFileSystemPort for LocalNoFollowFileSystem {
    fn capture<'a>(
        &'a self,
        root: &'a Path,
        paths: &'a [String],
        byte_limit: u64,
    ) -> MutationFuture<'a, Result<CapturedWorkspace, FileSystemMutationError>> {
        Box::pin(async move { capture_workspace(root, paths, byte_limit) })
    }

    fn apply<'a>(
        &'a self,
        root: &'a Path,
        plan: &'a PreparedChangeBatchPlan,
        preimages: &'a CapturedWorkspace,
    ) -> MutationFuture<'a, Result<AppliedMutationReport, FileSystemMutationError>> {
        Box::pin(async move {
            let cwd = PathUri::from_host_native_path(root)
                .map_err(|_| FileSystemMutationError::Unavailable)?;
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let delta = apply_patch_with_options(
                &plan.event().proposal.patch,
                ApplyPatchOptions {
                    update_file_mode: ApplyPatchFileUpdateMode::PreserveLineEndings,
                    follow_symlinks: false,
                },
                &cwd,
                &mut stdout,
                &mut stderr,
                self.executor.as_ref(),
                None,
            )
            .await
            .map_err(|_| FileSystemMutationError::Apply)?;
            let report = mutation_report(root, &delta)?;
            apply_canonical_modes(root, plan, preimages)?;
            Ok(report)
        })
    }

    fn restore_if_matches<'a>(
        &'a self,
        root: &'a Path,
        path: &'a str,
        expected: &'a CapturedFile,
        before: &'a CapturedFile,
    ) -> MutationFuture<'a, Result<RestoreStep, FileSystemMutationError>> {
        Box::pin(async move { restore_path(root, path, expected, before) })
    }
}

fn capture_workspace(
    root: &Path,
    paths: &[String],
    byte_limit: u64,
) -> Result<CapturedWorkspace, FileSystemMutationError> {
    let mut files = BTreeMap::new();
    let mut total_bytes = 0_u64;
    for path in paths {
        let remaining = byte_limit
            .checked_sub(total_bytes)
            .ok_or(FileSystemMutationError::ByteLimit)?;
        let state = capture_file(&root.join(path), remaining)?;
        total_bytes = total_bytes
            .checked_add(state.byte_len())
            .ok_or(FileSystemMutationError::ByteLimit)?;
        if total_bytes > byte_limit {
            return Err(FileSystemMutationError::ByteLimit);
        }
        files.insert(path.clone(), state);
    }
    Ok(CapturedWorkspace { files, total_bytes })
}

fn capture_file(path: &Path, byte_limit: u64) -> Result<CapturedFile, FileSystemMutationError> {
    let Some((parent, leaf)) = open_parent_allow_missing(path)? else {
        // Creating previously absent ancestor directories would add an
        // unplanned workspace delta that cannot be represented or rolled back
        // by the file-only canonical contract.
        return Err(FileSystemMutationError::InvalidEntry);
    };
    let fd = match openat(
        &parent,
        &leaf,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(error) if io::Error::from(error).kind() == io::ErrorKind::NotFound => {
            return Ok(CapturedFile::Absent);
        }
        Err(_) => return Err(FileSystemMutationError::InvalidEntry),
    };
    let stat = fstat(&fd).map_err(|_| FileSystemMutationError::Unavailable)?;
    let mut file = File::from(fd);
    let metadata = file
        .metadata()
        .map_err(|_| FileSystemMutationError::Unavailable)?;
    if !metadata.is_file() || stat.st_nlink != 1 {
        return Err(FileSystemMutationError::InvalidEntry);
    }
    let mode = u32::from(stat.st_mode) & 0o7777;
    if !matches!(mode, ADDED_FILE_MODE | EXECUTABLE_FILE_MODE) {
        return Err(FileSystemMutationError::InvalidEntry);
    }
    if metadata.len() > byte_limit {
        return Err(FileSystemMutationError::ByteLimit);
    }
    let take_limit = byte_limit.saturating_add(1);
    let mut bytes = Vec::new();
    io::Read::by_ref(&mut file)
        .take(take_limit)
        .read_to_end(&mut bytes)
        .map_err(|_| FileSystemMutationError::Unavailable)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > byte_limit {
        return Err(FileSystemMutationError::ByteLimit);
    }
    if std::str::from_utf8(&bytes).is_err() || looks_binary(&bytes) {
        return Err(FileSystemMutationError::InvalidEntry);
    }
    Ok(CapturedFile::Regular {
        digest: digest_bytes(&bytes),
        bytes,
        mode,
    })
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|byte| matches!(*byte, 0..=8 | 11 | 12 | 14..=31 | 127))
}

fn open_parent_allow_missing(
    path: &Path,
) -> Result<Option<(OwnedFd, std::ffi::OsString)>, FileSystemMutationError> {
    if !path.is_absolute() {
        return Err(FileSystemMutationError::InvalidEntry);
    }
    let mut parts = path
        .components()
        .filter_map(|part| match part {
            Component::RootDir => None,
            Component::Normal(name) => Some(Ok(name.to_os_string())),
            _ => Some(Err(FileSystemMutationError::InvalidEntry)),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let leaf = parts.pop().ok_or(FileSystemMutationError::InvalidEntry)?;
    let mut directory = open(
        "/",
        directory_access_flags() | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| FileSystemMutationError::Unavailable)?;
    for part in parts {
        match openat(
            &directory,
            part,
            directory_access_flags() | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(next) => directory = next,
            Err(error) if io::Error::from(error).kind() == io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(_) => return Err(FileSystemMutationError::InvalidEntry),
        }
    }
    Ok(Some((directory, leaf)))
}

fn directory_access_flags() -> OFlags {
    OFlags::RDONLY
}

fn mutation_report(
    root: &Path,
    delta: &AppliedPatchDelta,
) -> Result<AppliedMutationReport, FileSystemMutationError> {
    let mut operations = BTreeSet::new();
    let mut before_digests = BTreeMap::new();
    let mut after_digests = BTreeMap::new();
    for change in delta.changes() {
        let path = relative_uri_path(root, &change.path)?;
        let key = match &change.change {
            AppliedPatchFileChange::Add {
                content,
                overwritten_content,
            } => {
                if overwritten_content.is_some() {
                    return Err(FileSystemMutationError::Apply);
                }
                insert_report_digest(&mut after_digests, &path, content)?;
                OperationKey::Add(path)
            }
            AppliedPatchFileChange::Delete { content } => {
                insert_report_digest(&mut before_digests, &path, content)?;
                OperationKey::Delete(path)
            }
            AppliedPatchFileChange::Update {
                move_path,
                overwritten_move_content,
                old_content,
                new_content,
            } => {
                if let Some(destination) = move_path {
                    if overwritten_move_content.is_some() {
                        return Err(FileSystemMutationError::Apply);
                    }
                    let destination = relative_uri_path(root, destination)?;
                    insert_report_digest(&mut before_digests, &path, old_content)?;
                    insert_report_digest(&mut after_digests, &destination, new_content)?;
                    OperationKey::Move(path, destination)
                } else {
                    insert_report_digest(&mut before_digests, &path, old_content)?;
                    insert_report_digest(&mut after_digests, &path, new_content)?;
                    OperationKey::Update(path)
                }
            }
        };
        if !operations.insert(key) {
            return Err(FileSystemMutationError::Apply);
        }
    }
    Ok(AppliedMutationReport {
        exact: delta.is_exact(),
        operations,
        before_digests,
        after_digests,
    })
}

fn insert_report_digest(
    digests: &mut BTreeMap<String, Sha256Digest>,
    path: &str,
    contents: &str,
) -> Result<(), FileSystemMutationError> {
    if digests
        .insert(path.to_owned(), digest_bytes(contents.as_bytes()))
        .is_some()
    {
        return Err(FileSystemMutationError::Apply);
    }
    Ok(())
}

fn workspace_digests(workspace: &CapturedWorkspace) -> BTreeMap<String, Sha256Digest> {
    workspace
        .files
        .iter()
        .filter_map(|(path, state)| state.digest().cloned().map(|digest| (path.clone(), digest)))
        .collect()
}

fn relative_uri_path(root: &Path, path: &PathUri) -> Result<String, FileSystemMutationError> {
    let absolute = path
        .to_abs_path()
        .map_err(|_| FileSystemMutationError::Apply)?;
    let relative = absolute
        .as_path()
        .strip_prefix(root)
        .map_err(|_| FileSystemMutationError::Apply)?;
    relative
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or(FileSystemMutationError::Apply)
}

fn apply_canonical_modes(
    root: &Path,
    plan: &PreparedChangeBatchPlan,
    preimages: &CapturedWorkspace,
) -> Result<(), FileSystemMutationError> {
    for operation in plan.operations() {
        match operation {
            PlannedFileChange::Add { path } => {
                set_mode_no_follow(&root.join(path), ADDED_FILE_MODE)?;
            }
            PlannedFileChange::Move {
                source_path,
                destination_path,
                ..
            } => {
                let mode = preimages
                    .files
                    .get(source_path)
                    .and_then(CapturedFile::mode)
                    .ok_or(FileSystemMutationError::Apply)?;
                set_mode_no_follow(&root.join(destination_path), mode)?;
            }
            PlannedFileChange::Update { .. } | PlannedFileChange::Delete { .. } => {}
        }
    }
    Ok(())
}

fn set_mode_no_follow(path: &Path, mode: u32) -> Result<(), FileSystemMutationError> {
    let Some((parent, leaf)) = open_parent_allow_missing(path)? else {
        return Err(FileSystemMutationError::Apply);
    };
    let fd = openat(
        &parent,
        leaf,
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| FileSystemMutationError::Apply)?;
    let stat = fstat(&fd).map_err(|_| FileSystemMutationError::Apply)?;
    if stat.st_nlink != 1 {
        return Err(FileSystemMutationError::Apply);
    }
    rustix::fs::fchmod(&fd, rustix_mode(mode)?).map_err(|_| FileSystemMutationError::Apply)
}

fn restore_path(
    root: &Path,
    path: &str,
    expected: &CapturedFile,
    before: &CapturedFile,
) -> Result<RestoreStep, FileSystemMutationError> {
    let absolute = root.join(path);
    let current = capture_file(
        &absolute,
        expected.byte_len().max(before.byte_len()).saturating_add(1),
    )?;
    if &current == before {
        return Ok(RestoreStep::AlreadyOriginal);
    }
    if &current != expected {
        return Ok(RestoreStep::CasMismatch);
    }
    match before {
        CapturedFile::Absent => remove_regular_no_follow(&absolute)?,
        CapturedFile::Regular { bytes, mode, .. } => {
            write_exact_no_follow(&absolute, expected, bytes, *mode)?;
        }
    }
    let restored = capture_file(&absolute, before.byte_len().saturating_add(1))?;
    if &restored == before {
        Ok(RestoreStep::Restored)
    } else {
        Err(FileSystemMutationError::CasMismatch)
    }
}

fn remove_regular_no_follow(path: &Path) -> Result<(), FileSystemMutationError> {
    let Some((parent, leaf)) = open_parent_allow_missing(path)? else {
        return Err(FileSystemMutationError::CasMismatch);
    };
    unlinkat(&parent, leaf, rustix::fs::AtFlags::empty())
        .map_err(|_| FileSystemMutationError::CasMismatch)
}

fn write_exact_no_follow(
    path: &Path,
    expected: &CapturedFile,
    bytes: &[u8],
    mode: u32,
) -> Result<(), FileSystemMutationError> {
    let Some((parent, leaf)) = open_parent_allow_missing(path)? else {
        return Err(FileSystemMutationError::CasMismatch);
    };
    let flags = match expected {
        CapturedFile::Absent => {
            OFlags::WRONLY
                | OFlags::CREATE
                | OFlags::EXCL
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC
        }
        CapturedFile::Regular { .. } => {
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC
        }
    };
    let fd = openat(&parent, leaf, flags, rustix_mode(mode)?)
        .map_err(|_| FileSystemMutationError::CasMismatch)?;
    let mut file = File::from(fd);
    if !file
        .metadata()
        .map_err(|_| FileSystemMutationError::CasMismatch)?
        .is_file()
    {
        return Err(FileSystemMutationError::CasMismatch);
    }
    if matches!(expected, CapturedFile::Regular { .. }) {
        let mut current = Vec::new();
        file.read_to_end(&mut current)
            .map_err(|_| FileSystemMutationError::CasMismatch)?;
        if expected.digest() != Some(&digest_bytes(&current)) {
            return Err(FileSystemMutationError::CasMismatch);
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|_| FileSystemMutationError::CasMismatch)?;
        file.set_len(0)
            .map_err(|_| FileSystemMutationError::CasMismatch)?;
    }
    file.write_all(bytes)
        .map_err(|_| FileSystemMutationError::Unavailable)?;
    rustix::fs::fchmod(&file, rustix_mode(mode)?)
        .map_err(|_| FileSystemMutationError::Unavailable)?;
    file.sync_all()
        .map_err(|_| FileSystemMutationError::Unavailable)
}

fn digest_bytes(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn format_mode(mode: u32) -> String {
    format!("{mode:04o}")
}

fn rustix_mode(mode: u32) -> Result<Mode, FileSystemMutationError> {
    let raw =
        rustix::fs::RawMode::try_from(mode).map_err(|_| FileSystemMutationError::InvalidEntry)?;
    Ok(Mode::from_raw_mode(raw))
}

fn frame(hasher: &mut Sha256, bytes: &[u8]) {
    frame_u64(hasher, u64::try_from(bytes.len()).unwrap_or(u64::MAX));
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

#[cfg(test)]
mod tests;
