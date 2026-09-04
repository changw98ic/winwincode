// SPDX-License-Identifier: Apache-2.0

//! Private-index Git tree checkpoints for Worker workspaces.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fmt,
    fs::{self, File},
    io::{Read as _, Write as _},
    os::fd::OwnedFd,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use rustix::fs::{Mode, OFlags, fchmod, fstat, fsync, open, openat, renameat, unlinkat};
use sha2::{Digest as _, Sha256};
use winwincode_change_batch::{MAX_FILES, canonical_applied_file_summaries, derive_delta_digest};
use winwincode_domain::{Sha256Digest, WorkspaceRevision};
use winwincode_execution_port::generated::{AppliedFileOperation, AppliedFileSummary};

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const REGULAR_MODE: u32 = 0o644;
const EXECUTABLE_MODE: u32 = 0o755;
const MAX_PATH_BYTES: usize = 4096;
static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Stable failure classes exposed to Worker orchestration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceTreeErrorCode {
    InvalidInput,
    UnsafeWorkspace,
    Git,
    DeltaMismatch,
    Journal,
    Io,
}

/// Secret-safe workspace tree failure.
#[derive(Debug)]
pub(crate) struct WorkspaceTreeError {
    code: WorkspaceTreeErrorCode,
    message: &'static str,
}

impl WorkspaceTreeError {
    const fn new(code: WorkspaceTreeErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    /// Returns the stable category without exposing paths or file contents.
    #[cfg(test)]
    pub(crate) const fn code(&self) -> WorkspaceTreeErrorCode {
        self.code
    }

    /// Converts a durable-store failure without leaking database details.
    pub(crate) const fn journal() -> Self {
        Self::new(
            WorkspaceTreeErrorCode::Journal,
            "workspace tree restore journal is unavailable",
        )
    }
}

impl fmt::Display for WorkspaceTreeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for WorkspaceTreeError {}

/// Durable restore intent. The referenced trees contain every rollback byte.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceTreeRestoreIntent {
    workspace_id: String,
    expected_current: WorkspaceRevision,
    target: WorkspaceRevision,
}

impl WorkspaceTreeRestoreIntent {
    pub(crate) fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub(crate) const fn expected_current(&self) -> &WorkspaceRevision {
        &self.expected_current
    }

    pub(crate) const fn target(&self) -> &WorkspaceRevision {
        &self.target
    }
}

/// Journal seam that must fsync the restore intent before the first file write.
pub(crate) trait WorkspaceTreeRestoreJournalPort {
    fn persist_restore_intent_and_sync(
        &mut self,
        intent: &WorkspaceTreeRestoreIntent,
    ) -> Result<(), WorkspaceTreeError>;
}

/// Result of comparing the checkout with one exact tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceTreeComparison {
    Exact,
    Different,
    StateUncertain,
}

/// Result of a requested accepted-tree restoration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceTreeRestoreOutcome {
    AlreadyAtTarget,
    ExactRestored,
    ExactRolledBack,
    StateUncertain,
}

/// Exact post-Writer checkpoint derived from the accepted tree and pre-Writer tree.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WorkspaceWriterSnapshotOutcome {
    /// Writer commands made no workspace change.
    Unchanged {
        revision: WorkspaceRevision,
        files: Vec<AppliedFileSummary>,
        delta_digest: Sha256Digest,
    },
    /// Writer commands changed only authorized paths.
    Normalized {
        revision: WorkspaceRevision,
        files: Vec<AppliedFileSummary>,
        delta_digest: Sha256Digest,
        changed_file_digests: Vec<Sha256Digest>,
    },
    /// Writer commands changed a path outside their exact authority.
    ScopeViolation {
        observed_revision: WorkspaceRevision,
    },
    /// The checkout could not be enumerated without following an unsafe object.
    StateUncertain,
}

/// Deep Worker-private module for exact Git tree computation and restoration.
#[derive(Debug)]
pub(crate) struct WorkspaceTreeStore {
    checkout: PathBuf,
    state_root: PathBuf,
    git_dir: PathBuf,
    source_objects: PathBuf,
    private_objects: PathBuf,
    private_index: PathBuf,
    private_home: PathBuf,
    object_id_len: usize,
    fail_after_write: std::cell::Cell<Option<usize>>,
}

impl WorkspaceTreeStore {
    /// Opens an isolated tree store. `state_root` must be outside the checkout.
    pub(crate) fn open(
        checkout: impl AsRef<Path>,
        state_root: impl AsRef<Path>,
    ) -> Result<Self, WorkspaceTreeError> {
        let checkout = fs::canonicalize(checkout).map_err(|_| io_error())?;
        ensure_directory_no_link(&checkout)?;
        let requested_state = absolute_path(state_root.as_ref())?;
        if requested_state.starts_with(&checkout) {
            return Err(WorkspaceTreeError::new(
                WorkspaceTreeErrorCode::UnsafeWorkspace,
                "workspace tree state must be outside the checkout",
            ));
        }
        create_private_directory(&requested_state)?;
        let state_root = fs::canonicalize(requested_state).map_err(|_| io_error())?;
        if state_root.starts_with(&checkout) || checkout.starts_with(&state_root) {
            return Err(WorkspaceTreeError::new(
                WorkspaceTreeErrorCode::UnsafeWorkspace,
                "workspace tree state and checkout must be disjoint",
            ));
        }

        let bootstrap = GitEnvironment::bootstrap(&checkout, &state_root);
        let git_dir_text =
            bootstrap.run_text(["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        let git_dir = fs::canonicalize(git_dir_text.trim()).map_err(|_| git_error())?;
        let source_objects = fs::canonicalize(git_dir.join("objects")).map_err(|_| git_error())?;
        ensure_directory_no_link(&source_objects)?;
        let format = bootstrap.run_text(["rev-parse", "--show-object-format"])?;
        let object_id_len = match format.trim() {
            "sha1" => 40,
            "sha256" => 64,
            _ => return Err(git_error()),
        };

        let private_git_dir = state_root.join("git");
        initialize_private_git(&bootstrap, &private_git_dir, format.trim())?;
        let private_objects = state_root.join("objects");
        let private_home = state_root.join("home");
        create_private_directory(&private_objects)?;
        create_private_directory(&private_home)?;
        let private_index = state_root.join("index");
        reject_link_if_present(&private_index)?;
        Ok(Self {
            checkout,
            state_root,
            git_dir: private_git_dir,
            source_objects,
            private_objects,
            private_index,
            private_home,
            object_id_len,
            fail_after_write: std::cell::Cell::new(None),
        })
    }

    /// Computes a candidate tree without touching the real index or `HEAD`.
    /// The actual base-to-result delta must exactly equal the supplied receipt.
    pub(crate) fn compute_tree(
        &self,
        base: &WorkspaceRevision,
        files: &[AppliedFileSummary],
        delta_digest: &Sha256Digest,
    ) -> Result<WorkspaceRevision, WorkspaceTreeError> {
        let base_id = self.revision_id(base)?;
        self.require_tree(base_id)?;
        let result_id = self.compute_working_tree(base_id)?;
        let expected = canonical_applied_file_summaries(files).map_err(|_| delta_mismatch())?;
        if derive_delta_digest(&expected).map_err(|_| delta_mismatch())? != *delta_digest {
            return Err(delta_mismatch());
        }
        let actual = self.summarize_delta(base_id, &result_id, &expected)?;
        if actual != expected
            || derive_delta_digest(&actual).map_err(|_| delta_mismatch())? != *delta_digest
        {
            return Err(delta_mismatch());
        }
        self.sync_private_objects()?;
        self.parse_revision(&format!("git-tree:{result_id}"))
    }

    /// Reads one regular-file blob from an exact accepted tree, never the checkout.
    ///
    /// # Errors
    ///
    /// Rejects an invalid revision/path, a symbolic-link or gitlink tree, or a
    /// blob larger than `maximum_bytes`.
    pub(crate) fn read_blob_at_revision(
        &self,
        revision: &WorkspaceRevision,
        path: &str,
        maximum_bytes: usize,
    ) -> Result<Option<Vec<u8>>, WorkspaceTreeError> {
        if maximum_bytes == 0 {
            return Err(invalid_input());
        }
        validate_path(path)?;
        let revision_id = self.revision_id(revision)?;
        self.require_tree(revision_id)?;
        let entries = self.read_tree(revision_id)?;
        let Some(entry) = entries.get(path) else {
            return Ok(None);
        };
        let bytes = self.read_blob(&entry.object_id)?;
        if bytes.len() > maximum_bytes {
            return Err(invalid_input());
        }
        Ok(Some(bytes))
    }

    /// Captures the post-Writer tree and proves both Writer scope and the full
    /// accepted-base-to-result receipt.
    ///
    /// `pre_writer` is the exact PR3 checkpoint. `applied_files` is its
    /// accepted-base delta, retained so Move operations keep their canonical
    /// identity. Writer changes must be a subset of `allowed_writer_paths`.
    ///
    /// # Errors
    ///
    /// Rejects invalid trees, paths, receipt deltas, or a final delta that
    /// cannot fit the canonical 20-file receipt.
    pub(crate) fn snapshot_writer_changes(
        &self,
        accepted_base: &WorkspaceRevision,
        pre_writer: &WorkspaceRevision,
        applied_files: &[AppliedFileSummary],
        allowed_writer_paths: &[String],
    ) -> Result<WorkspaceWriterSnapshotOutcome, WorkspaceTreeError> {
        let accepted_id = self.revision_id(accepted_base)?;
        let pre_writer_id = self.revision_id(pre_writer)?;
        self.require_tree(accepted_id)?;
        self.require_tree(pre_writer_id)?;
        let canonical_applied =
            canonical_applied_file_summaries(applied_files).map_err(|_| delta_mismatch())?;
        let expected_pre_writer =
            self.summarize_complete_delta(accepted_id, pre_writer_id, &canonical_applied)?;
        if expected_pre_writer != canonical_applied {
            return Err(delta_mismatch());
        }
        let allowed = canonical_paths(allowed_writer_paths)?;
        let result_id = match self.compute_working_tree(pre_writer_id) {
            Ok(result) => result,
            Err(error) if error.code == WorkspaceTreeErrorCode::UnsafeWorkspace => {
                return Ok(WorkspaceWriterSnapshotOutcome::StateUncertain);
            }
            Err(error) => return Err(error),
        };
        self.sync_private_objects()?;
        let revision = self.parse_revision(&format!("git-tree:{result_id}"))?;
        let writer_delta = match self.summarize_inferred_delta(pre_writer_id, &result_id) {
            Ok(delta) => delta,
            Err(error) if error.code == WorkspaceTreeErrorCode::DeltaMismatch => {
                return Ok(WorkspaceWriterSnapshotOutcome::ScopeViolation {
                    observed_revision: revision,
                });
            }
            Err(error) => return Err(error),
        };
        let writer_paths: BTreeSet<_> = writer_delta
            .iter()
            .flat_map(|summary| {
                std::iter::once(summary.path.clone()).chain(summary.move_path.clone())
            })
            .collect();
        if !writer_paths.is_subset(&allowed) {
            return Ok(WorkspaceWriterSnapshotOutcome::ScopeViolation {
                observed_revision: revision,
            });
        }
        let final_files =
            match self.summarize_complete_delta(accepted_id, &result_id, &canonical_applied) {
                Ok(files) => files,
                Err(error) if error.code == WorkspaceTreeErrorCode::DeltaMismatch => {
                    return Ok(WorkspaceWriterSnapshotOutcome::ScopeViolation {
                        observed_revision: revision,
                    });
                }
                Err(error) => return Err(error),
            };
        if final_files.len() > MAX_FILES
            || (final_files.is_empty() && !canonical_applied.is_empty())
        {
            return Ok(WorkspaceWriterSnapshotOutcome::ScopeViolation {
                observed_revision: revision,
            });
        }
        let delta_digest = derive_delta_digest(&final_files).map_err(|_| delta_mismatch())?;
        if writer_delta.is_empty() {
            return Ok(WorkspaceWriterSnapshotOutcome::Unchanged {
                revision,
                files: final_files,
                delta_digest,
            });
        }
        let changed_file_digests = writer_delta.iter().map(change_summary_digest).collect();
        Ok(WorkspaceWriterSnapshotOutcome::Normalized {
            revision,
            files: final_files,
            delta_digest,
            changed_file_digests,
        })
    }

    /// Compares the managed checkout contents with an exact tree.
    pub(crate) fn compare_tree(
        &self,
        expected: &WorkspaceRevision,
    ) -> Result<WorkspaceTreeComparison, WorkspaceTreeError> {
        let expected_id = self.revision_id(expected)?;
        self.require_tree(expected_id)?;
        match self.compute_working_tree(expected_id) {
            Ok(actual) if actual == expected_id => Ok(WorkspaceTreeComparison::Exact),
            Ok(_) => Ok(WorkspaceTreeComparison::Different),
            Err(error) if error.code == WorkspaceTreeErrorCode::UnsafeWorkspace => {
                Ok(WorkspaceTreeComparison::StateUncertain)
            }
            Err(error) => Err(error),
        }
    }

    /// Restores one accepted tree with per-path before/after checks.
    ///
    /// The intent reaches durable storage before the first mutation. Ignored
    /// untracked paths remain outside both trees and are preserved. Any path
    /// whose state is neither the expected nor target version fails closed.
    pub(crate) fn restore_tree(
        &self,
        workspace_id: &str,
        expected_current: &WorkspaceRevision,
        target: &WorkspaceRevision,
        journal: &mut dyn WorkspaceTreeRestoreJournalPort,
    ) -> Result<WorkspaceTreeRestoreOutcome, WorkspaceTreeError> {
        validate_workspace_id(workspace_id)?;
        let expected_id = self.revision_id(expected_current)?;
        let target_id = self.revision_id(target)?;
        self.require_tree(expected_id)?;
        self.require_tree(target_id)?;
        let mut expected = self.read_tree(expected_id)?;
        let target_entries = self.read_tree(target_id)?;
        self.reset_index(expected_id)?;
        let mut all_paths: BTreeSet<_> = expected
            .keys()
            .chain(target_entries.keys())
            .cloned()
            .collect();
        all_paths.extend(self.managed_paths()?);
        let states = match self.capture_paths(&all_paths) {
            Ok(states) => states,
            Err(error) if error.code == WorkspaceTreeErrorCode::UnsafeWorkspace => {
                return Ok(WorkspaceTreeRestoreOutcome::StateUncertain);
            }
            Err(error) => return Err(error),
        };
        // A non-ignored untracked file is not represented by either Git tree,
        // but it is part of the exact current state that restore must remove.
        // Retaining its captured entry as the rollback preimage makes deletion
        // CAS-bound and lets a failed restore put the extra back exactly.
        for (path, state) in &states {
            if !expected.contains_key(path)
                && !target_entries.contains_key(path)
                && let Some(entry) = state
            {
                expected.insert(path.clone(), entry.clone());
            }
        }
        if states
            .iter()
            .all(|(path, state)| target_entries.get(path) == state.as_ref())
        {
            return Ok(WorkspaceTreeRestoreOutcome::AlreadyAtTarget);
        }
        if states.iter().any(|(path, state)| {
            let value = state.as_ref();
            value != expected.get(path) && value != target_entries.get(path)
        }) {
            return Ok(WorkspaceTreeRestoreOutcome::StateUncertain);
        }

        journal.persist_restore_intent_and_sync(&WorkspaceTreeRestoreIntent {
            workspace_id: workspace_id.to_owned(),
            expected_current: expected_current.clone(),
            target: target.clone(),
        })?;

        let mut completed = Vec::new();
        for path in &all_paths {
            if states.get(path).and_then(Option::as_ref) == target_entries.get(path) {
                continue;
            }
            if self.inject_write_fault(completed.len()) {
                return Ok(self.rollback(&completed, &expected, &target_entries));
            }
            if self
                .replace_path(path, expected.get(path), target_entries.get(path))
                .is_err()
            {
                return Ok(self.rollback(&completed, &expected, &target_entries));
            }
            completed.push(path.clone());
        }
        sync_directory(&self.checkout)?;
        Ok(WorkspaceTreeRestoreOutcome::ExactRestored)
    }

    fn rollback(
        &self,
        completed: &[String],
        expected: &BTreeMap<String, TreeEntry>,
        target: &BTreeMap<String, TreeEntry>,
    ) -> WorkspaceTreeRestoreOutcome {
        for path in completed.iter().rev() {
            if self
                .replace_path(path, target.get(path), expected.get(path))
                .is_err()
            {
                return WorkspaceTreeRestoreOutcome::StateUncertain;
            }
        }
        WorkspaceTreeRestoreOutcome::ExactRolledBack
    }

    fn compute_working_tree(
        &self,
        classification_tree: &str,
    ) -> Result<String, WorkspaceTreeError> {
        // Parsing the base first rejects symbolic-link and gitlink entries even
        // when their checkout path is absent and would otherwise look deleted.
        self.read_tree(classification_tree)?;
        self.reset_index(classification_tree)?;
        let paths = self.managed_paths()?;
        self.run_private(["read-tree", "--empty"], None)?;
        for path in paths {
            let Some(entry) = capture_one(&self.checkout, &path, self)? else {
                continue;
            };
            self.update_index(&path, entry.mode, &entry.object_id)?;
        }
        let tree = self.run_private_text(["write-tree"], None)?;
        let tree = tree.trim();
        if !valid_object_id(tree, self.object_id_len) {
            return Err(git_error());
        }
        Ok(tree.to_owned())
    }

    fn reset_index(&self, tree: &str) -> Result<(), WorkspaceTreeError> {
        reject_link_if_present(&self.private_index)?;
        match fs::remove_file(&self.private_index) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(io_error()),
        }
        self.run_private(["read-tree", tree], None).map(|_| ())
    }

    fn managed_paths(&self) -> Result<Vec<String>, WorkspaceTreeError> {
        let output = self.run_private(
            [
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-per-directory=.gitignore",
            ],
            None,
        )?;
        parse_nul_paths(&output.stdout)
    }

    fn hash_blob(&self, bytes: &[u8]) -> Result<String, WorkspaceTreeError> {
        let output = self.run_private(
            ["hash-object", "-w", "--no-filters", "--stdin"],
            Some(bytes),
        )?;
        let value = std::str::from_utf8(&output.stdout)
            .map_err(|_| git_error())?
            .trim();
        if !valid_object_id(value, self.object_id_len) {
            return Err(git_error());
        }
        Ok(value.to_owned())
    }

    fn update_index(
        &self,
        path: &str,
        mode: u32,
        object_id: &str,
    ) -> Result<(), WorkspaceTreeError> {
        let index_mode = match mode {
            REGULAR_MODE => "100644",
            EXECUTABLE_MODE => "100755",
            _ => return Err(unsafe_workspace()),
        };
        let mut line = format!("{index_mode} {object_id}\t").into_bytes();
        line.extend_from_slice(path.as_bytes());
        line.push(0);
        self.run_private(["update-index", "-z", "--index-info"], Some(&line))
            .map(|_| ())
    }

    fn summarize_delta(
        &self,
        base: &str,
        result: &str,
        expected: &[AppliedFileSummary],
    ) -> Result<Vec<AppliedFileSummary>, WorkspaceTreeError> {
        let before = self.read_tree(base)?;
        let after = self.read_tree(result)?;
        let expected_paths: BTreeSet<_> = expected
            .iter()
            .flat_map(|summary| {
                std::iter::once(summary.path.clone()).chain(summary.move_path.clone())
            })
            .collect();
        let changed_paths: BTreeSet<_> = before
            .keys()
            .chain(after.keys())
            .filter(|path| before.get(*path) != after.get(*path))
            .cloned()
            .collect();
        if changed_paths != expected_paths {
            return Err(delta_mismatch());
        }
        let mut actual = Vec::with_capacity(expected.len());
        for summary in expected {
            actual.push(self.summary_for_expected(summary, &before, &after)?);
        }
        canonical_applied_file_summaries(&actual).map_err(|_| delta_mismatch())
    }

    fn summarize_inferred_delta(
        &self,
        base: &str,
        result: &str,
    ) -> Result<Vec<AppliedFileSummary>, WorkspaceTreeError> {
        let before = self.read_tree(base)?;
        let after = self.read_tree(result)?;
        let mut summaries = Vec::new();
        for path in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
            if before.get(path) == after.get(path) {
                continue;
            }
            let operation = match (before.get(path), after.get(path)) {
                (None, Some(_)) => AppliedFileOperation::Create,
                (Some(_), None) => AppliedFileOperation::Delete,
                (Some(_), Some(_)) => AppliedFileOperation::Update,
                (None, None) => continue,
            };
            let expected = AppliedFileSummary {
                path: path.clone(),
                operation,
                move_path: None,
                before_sha256: None,
                after_sha256: None,
                bytes_before: 0,
                bytes_after: 0,
                mode_before: None,
                mode_after: None,
            };
            summaries.push(self.summary_for_expected(&expected, &before, &after)?);
        }
        canonical_applied_file_summaries(&summaries).map_err(|_| delta_mismatch())
    }

    fn summarize_complete_delta(
        &self,
        base: &str,
        result: &str,
        applied_files: &[AppliedFileSummary],
    ) -> Result<Vec<AppliedFileSummary>, WorkspaceTreeError> {
        let before = self.read_tree(base)?;
        let after = self.read_tree(result)?;
        let changed_paths: BTreeSet<_> = before
            .keys()
            .chain(after.keys())
            .filter(|path| before.get(*path) != after.get(*path))
            .cloned()
            .collect();
        let mut consumed = BTreeSet::new();
        let mut summaries = Vec::new();
        for applied in applied_files {
            let represented = match applied.operation {
                AppliedFileOperation::Create => {
                    !before.contains_key(&applied.path) && after.contains_key(&applied.path)
                }
                AppliedFileOperation::Update => {
                    before.contains_key(&applied.path)
                        && after.contains_key(&applied.path)
                        && changed_paths.contains(&applied.path)
                }
                AppliedFileOperation::Delete => {
                    before.contains_key(&applied.path) && !after.contains_key(&applied.path)
                }
                AppliedFileOperation::MoveValue => {
                    let destination = applied.move_path.as_ref().ok_or_else(delta_mismatch)?;
                    before.contains_key(&applied.path)
                        && !after.contains_key(&applied.path)
                        && !before.contains_key(destination)
                        && after.contains_key(destination)
                }
            };
            if represented {
                let summary = self.summary_for_expected(applied, &before, &after)?;
                consumed.insert(applied.path.clone());
                if let Some(destination) = &applied.move_path {
                    consumed.insert(destination.clone());
                }
                summaries.push(summary);
                continue;
            }
            let fully_reverted = match applied.operation {
                AppliedFileOperation::Create
                | AppliedFileOperation::Update
                | AppliedFileOperation::Delete => {
                    before.get(&applied.path) == after.get(&applied.path)
                }
                AppliedFileOperation::MoveValue => {
                    let destination = applied.move_path.as_ref().ok_or_else(delta_mismatch)?;
                    before.get(&applied.path) == after.get(&applied.path)
                        && before.get(destination) == after.get(destination)
                }
            };
            if !fully_reverted {
                return Err(delta_mismatch());
            }
        }
        for path in changed_paths.difference(&consumed) {
            let operation = match (before.get(path), after.get(path)) {
                (None, Some(_)) => AppliedFileOperation::Create,
                (Some(_), None) => AppliedFileOperation::Delete,
                (Some(_), Some(_)) => AppliedFileOperation::Update,
                (None, None) => continue,
            };
            let expected = AppliedFileSummary {
                path: path.clone(),
                operation,
                move_path: None,
                before_sha256: None,
                after_sha256: None,
                bytes_before: 0,
                bytes_after: 0,
                mode_before: None,
                mode_after: None,
            };
            summaries.push(self.summary_for_expected(&expected, &before, &after)?);
        }
        canonical_applied_file_summaries(&summaries).map_err(|_| delta_mismatch())
    }

    fn summary_for_expected(
        &self,
        expected: &AppliedFileSummary,
        before: &BTreeMap<String, TreeEntry>,
        after: &BTreeMap<String, TreeEntry>,
    ) -> Result<AppliedFileSummary, WorkspaceTreeError> {
        let (before_entry, after_entry) = match expected.operation {
            AppliedFileOperation::Create => (None, after.get(&expected.path)),
            AppliedFileOperation::Update => (before.get(&expected.path), after.get(&expected.path)),
            AppliedFileOperation::Delete => (before.get(&expected.path), None),
            AppliedFileOperation::MoveValue => {
                let destination = expected.move_path.as_ref().ok_or_else(delta_mismatch)?;
                (before.get(&expected.path), after.get(destination))
            }
        };
        match expected.operation {
            AppliedFileOperation::Create if before.contains_key(&expected.path) => {
                return Err(delta_mismatch());
            }
            AppliedFileOperation::Delete if after.contains_key(&expected.path) => {
                return Err(delta_mismatch());
            }
            AppliedFileOperation::MoveValue => {
                let destination = expected.move_path.as_ref().ok_or_else(delta_mismatch)?;
                if after.contains_key(&expected.path) || before.contains_key(destination) {
                    return Err(delta_mismatch());
                }
            }
            _ => {}
        }
        let before_blob = before_entry
            .map(|entry| self.blob_summary(entry))
            .transpose()?;
        let after_blob = after_entry
            .map(|entry| self.blob_summary(entry))
            .transpose()?;
        Ok(AppliedFileSummary {
            path: expected.path.clone(),
            operation: expected.operation.clone(),
            move_path: expected.move_path.clone(),
            before_sha256: before_blob.as_ref().map(|blob| blob.digest.clone()),
            after_sha256: after_blob.as_ref().map(|blob| blob.digest.clone()),
            bytes_before: before_blob.as_ref().map_or(0, |blob| blob.bytes),
            bytes_after: after_blob.as_ref().map_or(0, |blob| blob.bytes),
            mode_before: before_entry.map(TreeEntry::receipt_mode),
            mode_after: after_entry.map(TreeEntry::receipt_mode),
        })
    }

    fn blob_summary(&self, entry: &TreeEntry) -> Result<BlobSummary, WorkspaceTreeError> {
        let bytes = self.read_blob(&entry.object_id)?;
        let byte_count = i64::try_from(bytes.len()).map_err(|_| delta_mismatch())?;
        Ok(BlobSummary {
            digest: sha256_digest(&bytes),
            bytes: byte_count,
        })
    }

    fn read_tree(&self, tree: &str) -> Result<BTreeMap<String, TreeEntry>, WorkspaceTreeError> {
        let output = self.run_private(["ls-tree", "-r", "-z", tree], None)?;
        parse_tree(&output.stdout, self.object_id_len)
    }

    fn read_blob(&self, object_id: &str) -> Result<Vec<u8>, WorkspaceTreeError> {
        Ok(self
            .run_private(["cat-file", "blob", object_id], None)?
            .stdout)
    }

    fn capture_paths(
        &self,
        paths: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, Option<TreeEntry>>, WorkspaceTreeError> {
        paths
            .iter()
            .map(|path| Ok((path.clone(), capture_one(&self.checkout, path, self)?)))
            .collect()
    }

    fn replace_path(
        &self,
        path: &str,
        expected: Option<&TreeEntry>,
        target: Option<&TreeEntry>,
    ) -> Result<(), WorkspaceTreeError> {
        let (parent, leaf) = open_parent_no_follow(&self.checkout, path)?;
        if capture_one_at(&parent, &leaf, self)?.as_ref() != expected {
            return Err(WorkspaceTreeError::new(
                WorkspaceTreeErrorCode::UnsafeWorkspace,
                "workspace path changed during tree restoration",
            ));
        }
        if let Some(entry) = target {
            let bytes = self.read_blob(&entry.object_id)?;
            atomic_replace_at(&parent, &leaf, &bytes, entry.mode)?;
        } else {
            unlinkat(&parent, &leaf, rustix::fs::AtFlags::empty()).map_err(|_| io_error())?;
            fsync(&parent).map_err(|_| io_error())?;
        }
        if capture_one_at(&parent, &leaf, self)?.as_ref() != target {
            return Err(WorkspaceTreeError::new(
                WorkspaceTreeErrorCode::UnsafeWorkspace,
                "workspace path did not reach the requested tree state",
            ));
        }
        Ok(())
    }

    fn require_tree(&self, object_id: &str) -> Result<(), WorkspaceTreeError> {
        let kind = self.run_private_text(["cat-file", "-t", object_id], None)?;
        if kind.trim() != "tree" {
            return Err(git_error());
        }
        Ok(())
    }

    fn revision_id<'a>(
        &self,
        revision: &'a WorkspaceRevision,
    ) -> Result<&'a str, WorkspaceTreeError> {
        let object_id = revision
            .0
            .strip_prefix("git-tree:")
            .ok_or_else(invalid_input)?;
        if !valid_object_id(object_id, self.object_id_len) {
            return Err(invalid_input());
        }
        Ok(object_id)
    }

    fn parse_revision(&self, value: &str) -> Result<WorkspaceRevision, WorkspaceTreeError> {
        let object_id = value.strip_prefix("git-tree:").ok_or_else(invalid_input)?;
        if !valid_object_id(object_id, self.object_id_len) {
            return Err(invalid_input());
        }
        Ok(WorkspaceRevision(value.to_owned()))
    }

    fn run_private<const N: usize>(
        &self,
        args: [&str; N],
        stdin: Option<&[u8]>,
    ) -> Result<Output, WorkspaceTreeError> {
        GitEnvironment::private(self).run(args, stdin)
    }

    fn run_private_text<const N: usize>(
        &self,
        args: [&str; N],
        stdin: Option<&[u8]>,
    ) -> Result<String, WorkspaceTreeError> {
        let output = self.run_private(args, stdin)?;
        String::from_utf8(output.stdout).map_err(|_| git_error())
    }

    fn sync_private_objects(&self) -> Result<(), WorkspaceTreeError> {
        sync_tree(&self.private_objects)?;
        sync_directory(&self.state_root)
    }

    fn inject_write_fault(&self, completed: usize) -> bool {
        self.fail_after_write.get() == Some(completed)
    }
}

struct GitEnvironment<'a> {
    checkout: &'a Path,
    state_root: &'a Path,
    git_dir: Option<&'a Path>,
    source_objects: Option<&'a Path>,
    private_objects: Option<&'a Path>,
    private_index: Option<&'a Path>,
    private_home: Option<&'a Path>,
}

impl<'a> GitEnvironment<'a> {
    const fn bootstrap(checkout: &'a Path, state_root: &'a Path) -> Self {
        Self {
            checkout,
            state_root,
            git_dir: None,
            source_objects: None,
            private_objects: None,
            private_index: None,
            private_home: None,
        }
    }

    fn private(store: &'a WorkspaceTreeStore) -> Self {
        Self {
            checkout: &store.checkout,
            state_root: &store.state_root,
            git_dir: Some(&store.git_dir),
            source_objects: Some(&store.source_objects),
            private_objects: Some(&store.private_objects),
            private_index: Some(&store.private_index),
            private_home: Some(&store.private_home),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        clear_inherited_git_environment(&mut command);
        command
            .current_dir(self.checkout)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("HOME", self.private_home.unwrap_or(self.state_root))
            .env(
                "XDG_CONFIG_HOME",
                self.private_home.unwrap_or(self.state_root),
            )
            .env("LC_ALL", "C")
            .args([
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.filemode=true",
                "-c",
                "core.safecrlf=false",
                "-c",
                "filter.lfs.clean=",
                "-c",
                "filter.lfs.smudge=",
                "-c",
                "filter.lfs.required=false",
            ]);
        if let Some(git_dir) = self.git_dir {
            command
                .env("GIT_DIR", git_dir)
                .env("GIT_WORK_TREE", self.checkout);
        }
        if let Some(index) = self.private_index {
            command.env("GIT_INDEX_FILE", index);
        }
        if let Some(objects) = self.private_objects {
            command.env("GIT_OBJECT_DIRECTORY", objects);
        }
        if let Some(alternate) = self.source_objects {
            command.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternate);
        }
        command
    }

    fn run<const N: usize>(
        &self,
        args: [&str; N],
        stdin: Option<&[u8]>,
    ) -> Result<Output, WorkspaceTreeError> {
        let mut command = self.command();
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        let mut child = command.spawn().map_err(|_| git_error())?;
        if let Some(bytes) = stdin {
            child
                .stdin
                .take()
                .ok_or_else(git_error)?
                .write_all(bytes)
                .map_err(|_| git_error())?;
        }
        let output = child.wait_with_output().map_err(|_| git_error())?;
        if !output.status.success() {
            return Err(git_error());
        }
        Ok(output)
    }

    fn run_text<const N: usize>(&self, args: [&str; N]) -> Result<String, WorkspaceTreeError> {
        String::from_utf8(self.run(args, None)?.stdout).map_err(|_| git_error())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TreeEntry {
    mode: u32,
    object_id: String,
}

impl TreeEntry {
    fn receipt_mode(&self) -> String {
        format!("0{:o}", self.mode & 0o777)
    }
}

struct BlobSummary {
    digest: Sha256Digest,
    bytes: i64,
}

fn parse_tree(
    bytes: &[u8],
    object_id_len: usize,
) -> Result<BTreeMap<String, TreeEntry>, WorkspaceTreeError> {
    let mut entries = BTreeMap::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(git_error)?;
        let header = std::str::from_utf8(&record[..tab]).map_err(|_| git_error())?;
        let path = std::str::from_utf8(&record[tab + 1..]).map_err(|_| unsafe_workspace())?;
        validate_path(path)?;
        let mut fields = header.split(' ');
        let mode = fields.next().ok_or_else(git_error)?;
        let kind = fields.next().ok_or_else(git_error)?;
        let object_id = fields.next().ok_or_else(git_error)?;
        if fields.next().is_some()
            || kind != "blob"
            || !matches!(mode, "100644" | "100755")
            || !valid_object_id(object_id, object_id_len)
        {
            return Err(unsafe_workspace());
        }
        let mode = if mode == "100755" {
            EXECUTABLE_MODE
        } else {
            REGULAR_MODE
        };
        if entries
            .insert(
                path.to_owned(),
                TreeEntry {
                    mode,
                    object_id: object_id.to_owned(),
                },
            )
            .is_some()
        {
            return Err(git_error());
        }
    }
    Ok(entries)
}

fn parse_nul_paths(bytes: &[u8]) -> Result<Vec<String>, WorkspaceTreeError> {
    let mut paths = BTreeSet::new();
    for raw in bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(raw).map_err(|_| unsafe_workspace())?;
        validate_path(path)?;
        if !paths.insert(path.to_owned()) {
            return Err(git_error());
        }
    }
    Ok(paths.into_iter().collect())
}

fn validate_path(path: &str) -> Result<(), WorkspaceTreeError> {
    let candidate = Path::new(path);
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.contains(['\\', '\0', '<', '>', ':', '"', '|', '?', '*'])
        || candidate.is_absolute()
        || candidate.components().any(|component| {
            let Component::Normal(component) = component else {
                return true;
            };
            let component = component.to_string_lossy();
            component == ".git"
                || component.ends_with([' ', '.'])
                || windows_reserved_component(&component)
        })
    {
        return Err(unsafe_workspace());
    }
    let canonical = candidate
        .components()
        .map(Component::as_os_str)
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if canonical != path {
        return Err(unsafe_workspace());
    }
    Ok(())
}

fn validate_workspace_id(value: &str) -> Result<(), WorkspaceTreeError> {
    if value.is_empty()
        || value.len() > 200
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
    {
        return Err(invalid_input());
    }
    Ok(())
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

fn open_parent_no_follow(
    root: &Path,
    relative: &str,
) -> Result<(OwnedFd, String), WorkspaceTreeError> {
    validate_path(relative)?;
    let mut parts = Path::new(relative)
        .components()
        .map(|component| match component {
            Component::Normal(part) => part
                .to_str()
                .map(ToOwned::to_owned)
                .ok_or_else(unsafe_workspace),
            _ => Err(unsafe_workspace()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let leaf = parts.pop().ok_or_else(unsafe_workspace)?;
    let mut directory = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| unsafe_workspace())?;
    for part in parts {
        directory = openat(
            &directory,
            part,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| unsafe_workspace())?;
    }
    Ok((directory, leaf))
}

fn capture_one(
    root: &Path,
    relative: &str,
    store: &WorkspaceTreeStore,
) -> Result<Option<TreeEntry>, WorkspaceTreeError> {
    let (parent, leaf) = open_parent_no_follow(root, relative)?;
    capture_one_at(&parent, &leaf, store)
}

fn capture_one_at(
    parent: &OwnedFd,
    leaf: &str,
    store: &WorkspaceTreeStore,
) -> Result<Option<TreeEntry>, WorkspaceTreeError> {
    let fd = match openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(_) => return Err(unsafe_workspace()),
    };
    let stat = fstat(&fd).map_err(|_| io_error())?;
    let mut file = File::from(fd);
    let opened = file.metadata().map_err(|_| io_error())?;
    if !opened.is_file() || stat.st_nlink != 1 {
        return Err(unsafe_workspace());
    }
    let mode = match opened.mode() & 0o777 {
        REGULAR_MODE => REGULAR_MODE,
        EXECUTABLE_MODE => EXECUTABLE_MODE,
        _ => return Err(unsafe_workspace()),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|_| io_error())?;
    let after = file.metadata().map_err(|_| io_error())?;
    if after.dev() != opened.dev()
        || after.ino() != opened.ino()
        || after.len() != opened.len()
        || after.mtime() != opened.mtime()
        || after.mtime_nsec() != opened.mtime_nsec()
    {
        return Err(unsafe_workspace());
    }
    Ok(Some(TreeEntry {
        mode,
        object_id: store.hash_blob(&bytes)?,
    }))
}

fn atomic_replace_at(
    parent: &OwnedFd,
    leaf: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<(), WorkspaceTreeError> {
    let sequence = TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = format!(
        ".{leaf}.winwincode-tree-tmp-{}-{sequence}",
        std::process::id()
    );
    let result = (|| {
        let fd = openat(
            parent,
            &temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            rustix_mode(mode)?,
        )
        .map_err(|_| io_error())?;
        let mut file = File::from(fd);
        file.write_all(bytes).map_err(|_| io_error())?;
        fchmod(&file, rustix_mode(mode)?).map_err(|_| io_error())?;
        file.sync_all().map_err(|_| io_error())?;
        drop(file);
        renameat(parent, &temporary, parent, leaf).map_err(|_| io_error())?;
        fsync(parent).map_err(|_| io_error())
    })();
    if result.is_err() {
        let _ = unlinkat(parent, &temporary, rustix::fs::AtFlags::empty());
    }
    result
}

fn rustix_mode(mode: u32) -> Result<Mode, WorkspaceTreeError> {
    let raw = rustix::fs::RawMode::try_from(mode).map_err(|_| invalid_input())?;
    Ok(Mode::from_raw_mode(raw))
}

fn create_private_directory(path: &Path) -> Result<(), WorkspaceTreeError> {
    fs::create_dir_all(path).map_err(|_| io_error())?;
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
        .map_err(|_| io_error())?;
    ensure_directory_no_link(path)?;
    if fs::metadata(path).map_err(|_| io_error())?.mode() & 0o777 != PRIVATE_DIRECTORY_MODE {
        return Err(unsafe_workspace());
    }
    Ok(())
}

fn initialize_private_git(
    bootstrap: &GitEnvironment<'_>,
    git_dir: &Path,
    object_format: &str,
) -> Result<(), WorkspaceTreeError> {
    reject_link_if_present(git_dir)?;
    let mut command = bootstrap.command();
    let output = command
        .args(["init", "--bare"])
        .arg(format!("--object-format={object_format}"))
        .arg(git_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| git_error())?;
    if !output.status.success() {
        return Err(git_error());
    }
    fs::set_permissions(git_dir, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
        .map_err(|_| io_error())?;
    ensure_directory_no_link(git_dir)
}

fn ensure_directory_no_link(path: &Path) -> Result<(), WorkspaceTreeError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| io_error())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(unsafe_workspace());
    }
    Ok(())
}

fn reject_link_if_present(path: &Path) -> Result<(), WorkspaceTreeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(unsafe_workspace()),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(io_error()),
    }
}

fn sync_tree(root: &Path) -> Result<(), WorkspaceTreeError> {
    for entry in fs::read_dir(root).map_err(|_| io_error())? {
        let path = entry.map_err(|_| io_error())?.path();
        let metadata = fs::symlink_metadata(&path).map_err(|_| io_error())?;
        if metadata.file_type().is_symlink() {
            return Err(unsafe_workspace());
        }
        if metadata.is_dir() {
            sync_tree(&path)?;
        } else if metadata.is_file() {
            File::open(&path)
                .and_then(|file| file.sync_all())
                .map_err(|_| io_error())?;
        } else {
            return Err(unsafe_workspace());
        }
    }
    sync_directory(root)
}

fn sync_directory(path: &Path) -> Result<(), WorkspaceTreeError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| io_error())
}

fn clear_inherited_git_environment(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if is_git_environment_key(&key) {
            command.env_remove(key);
        }
    }
}

fn is_git_environment_key(key: &OsStr) -> bool {
    key.to_string_lossy().starts_with("GIT_")
}

fn null_device() -> &'static OsStr {
    OsStr::new("/dev/null")
}

fn absolute_path(path: &Path) -> Result<PathBuf, WorkspaceTreeError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|_| io_error())
    }
}

fn valid_object_id(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn sha256_digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn canonical_paths(paths: &[String]) -> Result<BTreeSet<String>, WorkspaceTreeError> {
    if paths.len() > MAX_FILES {
        return Err(invalid_input());
    }
    let mut canonical = BTreeSet::new();
    for path in paths {
        validate_path(path)?;
        if !canonical.insert(path.clone()) {
            return Err(invalid_input());
        }
    }
    Ok(canonical)
}

fn change_summary_digest(summary: &AppliedFileSummary) -> Sha256Digest {
    let mut hasher = Sha256::new();
    digest_frame(&mut hasher, b"winwincode.writer-file-change.v1");
    digest_frame(&mut hasher, summary.path.as_bytes());
    hasher.update([match summary.operation {
        AppliedFileOperation::Create => 0,
        AppliedFileOperation::Update => 1,
        AppliedFileOperation::Delete => 2,
        AppliedFileOperation::MoveValue => 3,
    }]);
    digest_optional(&mut hasher, summary.move_path.as_deref());
    digest_optional(
        &mut hasher,
        summary
            .before_sha256
            .as_ref()
            .map(|digest| digest.0.as_str()),
    );
    digest_optional(
        &mut hasher,
        summary
            .after_sha256
            .as_ref()
            .map(|digest| digest.0.as_str()),
    );
    digest_frame(&mut hasher, &summary.bytes_before.to_be_bytes());
    digest_frame(&mut hasher, &summary.bytes_after.to_be_bytes());
    digest_optional(&mut hasher, summary.mode_before.as_deref());
    digest_optional(&mut hasher, summary.mode_after.as_deref());
    Sha256Digest(format!("sha256:{:x}", hasher.finalize()))
}

fn digest_optional(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        None => hasher.update([0]),
        Some(value) => {
            hasher.update([1]);
            digest_frame(hasher, value.as_bytes());
        }
    }
}

fn digest_frame(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}

const fn invalid_input() -> WorkspaceTreeError {
    WorkspaceTreeError::new(
        WorkspaceTreeErrorCode::InvalidInput,
        "workspace tree revision is invalid",
    )
}

const fn unsafe_workspace() -> WorkspaceTreeError {
    WorkspaceTreeError::new(
        WorkspaceTreeErrorCode::UnsafeWorkspace,
        "workspace tree contains unsafe filesystem state",
    )
}

const fn git_error() -> WorkspaceTreeError {
    WorkspaceTreeError::new(
        WorkspaceTreeErrorCode::Git,
        "private Git tree operation failed",
    )
}

const fn delta_mismatch() -> WorkspaceTreeError {
    WorkspaceTreeError::new(
        WorkspaceTreeErrorCode::DeltaMismatch,
        "workspace tree delta does not match the applied receipt",
    )
}

const fn io_error() -> WorkspaceTreeError {
    WorkspaceTreeError::new(
        WorkspaceTreeErrorCode::Io,
        "workspace tree filesystem operation failed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    struct Fixture {
        _root: TempDir,
        checkout: PathBuf,
        state: PathBuf,
        source: WorkspaceRevision,
    }

    struct MemoryJournal {
        intents: Vec<WorkspaceTreeRestoreIntent>,
        fail: bool,
    }

    impl WorkspaceTreeRestoreJournalPort for MemoryJournal {
        fn persist_restore_intent_and_sync(
            &mut self,
            intent: &WorkspaceTreeRestoreIntent,
        ) -> Result<(), WorkspaceTreeError> {
            if self.fail {
                return Err(WorkspaceTreeError::new(
                    WorkspaceTreeErrorCode::Journal,
                    "test restore journal failed",
                ));
            }
            if !self.intents.contains(intent) {
                self.intents.push(intent.clone());
            }
            Ok(())
        }
    }

    fn git(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repository)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .output()
            .expect("run Git fixture command");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("Git fixture output")
            .trim()
            .to_owned()
    }

    fn write(path: &Path, bytes: &[u8], mode: u32) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent");
        }
        fs::write(path, bytes).expect("write fixture file");
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set fixture mode");
    }

    fn fixture(object_format: &str) -> Fixture {
        let root = tempfile::tempdir().expect("fixture root");
        let checkout = root.path().join("checkout");
        let state = root.path().join("state");
        fs::create_dir(&checkout).expect("checkout root");
        git(
            &checkout,
            &["init", &format!("--object-format={object_format}")],
        );
        git(&checkout, &["config", "user.name", "WinWinCode Test"]);
        git(&checkout, &["config", "user.email", "test@example.invalid"]);
        write(&checkout.join("update.txt"), b"before\r\n", REGULAR_MODE);
        write(&checkout.join("delete.txt"), b"delete\n", REGULAR_MODE);
        write(&checkout.join("move.sh"), b"#!/bin/sh\n", EXECUTABLE_MODE);
        write(&checkout.join(".gitignore"), b"*.ignored\n", REGULAR_MODE);
        git(&checkout, &["add", "--all"]);
        git(&checkout, &["commit", "-m", "base"]);
        let source = WorkspaceRevision(format!(
            "git-tree:{}",
            git(&checkout, &["rev-parse", "HEAD^{tree}"])
        ));
        Fixture {
            _root: root,
            checkout,
            state,
            source,
        }
    }

    fn summary(
        path: &str,
        operation: AppliedFileOperation,
        move_path: Option<&str>,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
        mode_before: Option<&str>,
        mode_after: Option<&str>,
    ) -> AppliedFileSummary {
        AppliedFileSummary {
            path: path.to_owned(),
            operation,
            move_path: move_path.map(str::to_owned),
            before_sha256: before.map(sha256_digest),
            after_sha256: after.map(sha256_digest),
            bytes_before: before.map_or(0, |bytes| i64::try_from(bytes.len()).expect("length")),
            bytes_after: after.map_or(0, |bytes| i64::try_from(bytes.len()).expect("length")),
            mode_before: mode_before.map(str::to_owned),
            mode_after: mode_after.map(str::to_owned),
        }
    }

    fn four_operation_delta() -> Vec<AppliedFileSummary> {
        vec![
            summary(
                "add.txt",
                AppliedFileOperation::Create,
                None,
                None,
                Some(b"added\n"),
                None,
                Some("0644"),
            ),
            summary(
                "delete.txt",
                AppliedFileOperation::Delete,
                None,
                Some(b"delete\n"),
                None,
                Some("0644"),
                None,
            ),
            summary(
                "move.sh",
                AppliedFileOperation::MoveValue,
                Some("moved.sh"),
                Some(b"#!/bin/sh\n"),
                Some(b"#!/bin/sh\n"),
                Some("0755"),
                Some("0755"),
            ),
            summary(
                "update.txt",
                AppliedFileOperation::Update,
                None,
                Some(b"before\r\n"),
                Some(b"after\r\n"),
                Some("0644"),
                Some("0644"),
            ),
        ]
    }

    fn apply_four_operations(checkout: &Path) {
        write(&checkout.join("add.txt"), b"added\n", REGULAR_MODE);
        fs::remove_file(checkout.join("delete.txt")).expect("delete fixture file");
        fs::rename(checkout.join("move.sh"), checkout.join("moved.sh")).expect("move fixture file");
        write(&checkout.join("update.txt"), b"after\r\n", REGULAR_MODE);
    }

    #[test]
    fn private_tree_exactly_matches_four_operations_without_touching_head_or_index() {
        let fixture = fixture("sha1");
        let index = fixture.checkout.join(".git/index");
        let index_before = fs::read(&index).expect("real index");
        let head_before = git(&fixture.checkout, &["rev-parse", "HEAD"]);
        write(
            &fixture.checkout.join("keep.ignored"),
            b"ignored\n",
            REGULAR_MODE,
        );
        apply_four_operations(&fixture.checkout);
        let files = four_operation_delta();
        let digest = derive_delta_digest(&files).expect("delta digest");
        let store =
            WorkspaceTreeStore::open(&fixture.checkout, &fixture.state).expect("tree store");

        let result = store
            .compute_tree(&fixture.source, &files, &digest)
            .expect("exact candidate tree");

        assert!(result.0.starts_with("git-tree:"));
        assert_eq!(result.0.len(), "git-tree:".len() + 40);
        assert_eq!(git(&fixture.checkout, &["rev-parse", "HEAD"]), head_before);
        assert_eq!(fs::read(index).expect("real index after"), index_before);
        assert_eq!(
            fs::read(fixture.checkout.join("keep.ignored")).expect("ignored file"),
            b"ignored\n"
        );
        assert_eq!(
            fs::metadata(&fixture.state).expect("state metadata").mode() & 0o777,
            PRIVATE_DIRECTORY_MODE
        );
    }

    #[test]
    fn accepted_revision_blob_read_ignores_mutated_checkout_bytes() {
        let mut fixture = fixture("sha1");
        write(
            &fixture.checkout.join(".winwincode/validation.toml"),
            b"schemaVersion = 1\n",
            REGULAR_MODE,
        );
        git(&fixture.checkout, &["add", ".winwincode/validation.toml"]);
        git(&fixture.checkout, &["commit", "-m", "validation config"]);
        fixture.source = WorkspaceRevision(format!(
            "git-tree:{}",
            git(&fixture.checkout, &["rev-parse", "HEAD^{tree}"])
        ));
        let store =
            WorkspaceTreeStore::open(&fixture.checkout, &fixture.state).expect("tree store");
        write(
            &fixture.checkout.join(".winwincode/validation.toml"),
            b"schemaVersion = 999\n",
            REGULAR_MODE,
        );

        assert_eq!(
            store
                .read_blob_at_revision(&fixture.source, ".winwincode/validation.toml", 262_144,)
                .expect("accepted config blob"),
            Some(b"schemaVersion = 1\n".to_vec())
        );
        assert_eq!(
            store
                .read_blob_at_revision(&fixture.source, "missing.toml", 262_144)
                .expect("missing blob"),
            None
        );
        assert_eq!(
            store
                .read_blob_at_revision(&fixture.source, ".winwincode/validation.toml", 4)
                .expect_err("oversized blob")
                .code(),
            WorkspaceTreeErrorCode::InvalidInput
        );
        assert!(
            store
                .read_blob_at_revision(&fixture.source, "../validation.toml", 262_144)
                .is_err()
        );
    }

    #[test]
    fn writer_snapshot_proves_scope_and_the_complete_accepted_delta() {
        let fixture = fixture("sha1");
        apply_four_operations(&fixture.checkout);
        let applied_files = four_operation_delta();
        let applied_digest = derive_delta_digest(&applied_files).expect("applied delta digest");
        let store =
            WorkspaceTreeStore::open(&fixture.checkout, &fixture.state).expect("tree store");
        let pre_writer = store
            .compute_tree(&fixture.source, &applied_files, &applied_digest)
            .expect("pre-Writer tree");

        let unchanged = store
            .snapshot_writer_changes(&fixture.source, &pre_writer, &applied_files, &[])
            .expect("unchanged snapshot");
        assert_eq!(
            unchanged,
            WorkspaceWriterSnapshotOutcome::Unchanged {
                revision: pre_writer.clone(),
                files: canonical_applied_file_summaries(&applied_files).expect("canonical files"),
                delta_digest: applied_digest,
            }
        );

        write(
            &fixture.checkout.join("update.txt"),
            b"formatted\r\n",
            REGULAR_MODE,
        );
        write(
            &fixture.checkout.join("generated.txt"),
            b"generated\n",
            REGULAR_MODE,
        );
        let normalized = store
            .snapshot_writer_changes(
                &fixture.source,
                &pre_writer,
                &applied_files,
                &["generated.txt".to_owned(), "update.txt".to_owned()],
            )
            .expect("normalized snapshot");
        let WorkspaceWriterSnapshotOutcome::Normalized {
            revision,
            files,
            delta_digest,
            changed_file_digests,
        } = normalized
        else {
            panic!("expected normalized snapshot");
        };
        assert_eq!(files.len(), 5);
        assert_eq!(
            derive_delta_digest(&files).expect("complete digest"),
            delta_digest
        );
        assert_eq!(changed_file_digests.len(), 2);
        assert_eq!(
            store.compare_tree(&revision).expect("result comparison"),
            WorkspaceTreeComparison::Exact
        );

        write(
            &fixture.checkout.join("outside.txt"),
            b"outside\n",
            REGULAR_MODE,
        );
        let violation = store
            .snapshot_writer_changes(
                &fixture.source,
                &pre_writer,
                &applied_files,
                &["generated.txt".to_owned(), "update.txt".to_owned()],
            )
            .expect("scope violation");
        let WorkspaceWriterSnapshotOutcome::ScopeViolation { observed_revision } = violation else {
            panic!("expected scope violation");
        };
        assert_eq!(
            store
                .compare_tree(&observed_revision)
                .expect("observed tree comparison"),
            WorkspaceTreeComparison::Exact
        );
    }

    #[test]
    fn delta_mismatch_and_unsafe_files_fail_closed() {
        let fixture = fixture("sha1");
        apply_four_operations(&fixture.checkout);
        write(
            &fixture.checkout.join("unexpected.txt"),
            b"extra\n",
            REGULAR_MODE,
        );
        let files = four_operation_delta();
        let digest = derive_delta_digest(&files).expect("delta digest");
        let store =
            WorkspaceTreeStore::open(&fixture.checkout, &fixture.state).expect("tree store");
        assert_eq!(
            store
                .compute_tree(&fixture.source, &files, &digest)
                .expect_err("unreported file")
                .code(),
            WorkspaceTreeErrorCode::DeltaMismatch
        );

        fs::remove_file(fixture.checkout.join("unexpected.txt")).expect("remove extra");
        fs::remove_file(fixture.checkout.join("add.txt")).expect("remove add");
        std::os::unix::fs::symlink("update.txt", fixture.checkout.join("add.txt"))
            .expect("fixture symlink");
        assert_eq!(
            store
                .compute_tree(&fixture.source, &files, &digest)
                .expect_err("symlink")
                .code(),
            WorkspaceTreeErrorCode::UnsafeWorkspace
        );
    }

    #[test]
    fn binary_blobs_are_exact_but_ancestor_symlinks_are_uncertain() {
        let mut fixture = fixture("sha1");
        let store =
            WorkspaceTreeStore::open(&fixture.checkout, &fixture.state).expect("tree store");

        write(
            &fixture.checkout.join("asset.bin"),
            &[0, 0xff, b'\n'],
            REGULAR_MODE,
        );
        git(&fixture.checkout, &["add", "asset.bin"]);
        git(&fixture.checkout, &["commit", "-m", "binary base"]);
        fixture.source = WorkspaceRevision(format!(
            "git-tree:{}",
            git(&fixture.checkout, &["rev-parse", "HEAD^{tree}"])
        ));
        assert_eq!(
            store.compare_tree(&fixture.source).expect("binary tree"),
            WorkspaceTreeComparison::Exact
        );
        write(&fixture.checkout.join("asset.bin"), &[0, 1], REGULAR_MODE);
        assert_eq!(
            store.compare_tree(&fixture.source).expect("changed binary"),
            WorkspaceTreeComparison::Different
        );

        write(
            &fixture.checkout.join("asset.bin"),
            &[0, 0xff, b'\n'],
            REGULAR_MODE,
        );
        write(
            &fixture.checkout.join("nested/file.txt"),
            b"inside\n",
            REGULAR_MODE,
        );
        git(&fixture.checkout, &["add", "nested/file.txt"]);
        git(&fixture.checkout, &["commit", "-m", "nested base"]);
        fixture.source = WorkspaceRevision(format!(
            "git-tree:{}",
            git(&fixture.checkout, &["rev-parse", "HEAD^{tree}"])
        ));
        fs::rename(
            fixture.checkout.join("nested"),
            fixture.checkout.join("nested-original"),
        )
        .expect("move tracked ancestor");
        std::os::unix::fs::symlink("nested-original", fixture.checkout.join("nested"))
            .expect("ancestor symlink");
        assert_eq!(
            store
                .compare_tree(&fixture.source)
                .expect("ancestor symlink"),
            WorkspaceTreeComparison::StateUncertain
        );
    }

    #[test]
    fn hardlinks_and_gitlinks_are_rejected() {
        let fixture = fixture("sha1");
        fs::hard_link(
            fixture.checkout.join("update.txt"),
            fixture.checkout.join("hard.txt"),
        )
        .expect("fixture hardlink");
        let store =
            WorkspaceTreeStore::open(&fixture.checkout, &fixture.state).expect("tree store");
        assert_eq!(
            store.compare_tree(&fixture.source).expect("comparison"),
            WorkspaceTreeComparison::StateUncertain
        );
        fs::remove_file(fixture.checkout.join("hard.txt")).expect("remove hardlink");

        let commit = git(&fixture.checkout, &["rev-parse", "HEAD"]);
        git(
            &fixture.checkout,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                &commit,
                "submodule",
            ],
        );
        git(&fixture.checkout, &["commit", "-m", "gitlink"]);
        let gitlink_tree = WorkspaceRevision(format!(
            "git-tree:{}",
            git(&fixture.checkout, &["rev-parse", "HEAD^{tree}"])
        ));
        assert_eq!(
            store.compare_tree(&gitlink_tree).expect("comparison"),
            WorkspaceTreeComparison::StateUncertain
        );
    }

    #[test]
    fn restore_is_durable_before_write_preserves_ignored_and_rolls_back_faults() {
        let fixture = fixture("sha1");
        apply_four_operations(&fixture.checkout);
        write(
            &fixture.checkout.join("keep.ignored"),
            b"ignored\n",
            REGULAR_MODE,
        );
        let files = four_operation_delta();
        let digest = derive_delta_digest(&files).expect("delta digest");
        let store =
            WorkspaceTreeStore::open(&fixture.checkout, &fixture.state).expect("tree store");
        let candidate = store
            .compute_tree(&fixture.source, &files, &digest)
            .expect("candidate tree");
        let mut journal = MemoryJournal {
            intents: Vec::new(),
            fail: false,
        };

        store.fail_after_write.set(Some(1));
        assert_eq!(
            store
                .restore_tree("workspace-1", &candidate, &fixture.source, &mut journal)
                .expect("fault rollback"),
            WorkspaceTreeRestoreOutcome::ExactRolledBack
        );
        assert_eq!(
            store
                .compare_tree(&candidate)
                .expect("candidate comparison"),
            WorkspaceTreeComparison::Exact
        );

        store.fail_after_write.set(None);
        assert_eq!(
            store
                .restore_tree("workspace-1", &candidate, &fixture.source, &mut journal)
                .expect("accepted restore"),
            WorkspaceTreeRestoreOutcome::ExactRestored
        );
        assert_eq!(journal.intents.len(), 1);
        assert_eq!(
            fs::read(fixture.checkout.join("keep.ignored")).expect("ignored extra"),
            b"ignored\n"
        );
        assert_eq!(
            store
                .compare_tree(&fixture.source)
                .expect("source comparison"),
            WorkspaceTreeComparison::Exact
        );
    }

    #[test]
    fn restore_removes_nonignored_extras_and_restores_them_after_a_fault() {
        let fixture = fixture("sha1");
        apply_four_operations(&fixture.checkout);
        let files = four_operation_delta();
        let digest = derive_delta_digest(&files).expect("delta digest");
        let store =
            WorkspaceTreeStore::open(&fixture.checkout, &fixture.state).expect("tree store");
        let candidate = store
            .compute_tree(&fixture.source, &files, &digest)
            .expect("candidate tree");
        write(
            &fixture.checkout.join("extra.txt"),
            b"untracked extra\n",
            REGULAR_MODE,
        );
        let mut journal = MemoryJournal {
            intents: Vec::new(),
            fail: false,
        };

        store.fail_after_write.set(Some(3));
        assert_eq!(
            store
                .restore_tree("workspace-1", &candidate, &fixture.source, &mut journal)
                .expect("fault rollback"),
            WorkspaceTreeRestoreOutcome::ExactRolledBack
        );
        assert_eq!(
            fs::read(fixture.checkout.join("extra.txt")).expect("rolled-back extra"),
            b"untracked extra\n"
        );

        store.fail_after_write.set(None);
        assert_eq!(
            store
                .restore_tree("workspace-1", &candidate, &fixture.source, &mut journal)
                .expect("accepted restore"),
            WorkspaceTreeRestoreOutcome::ExactRestored
        );
        assert!(!fixture.checkout.join("extra.txt").exists());
        assert_eq!(
            store
                .compare_tree(&fixture.source)
                .expect("source comparison"),
            WorkspaceTreeComparison::Exact
        );
    }

    #[test]
    fn journal_failure_has_zero_workspace_writes() {
        let fixture = fixture("sha1");
        apply_four_operations(&fixture.checkout);
        let files = four_operation_delta();
        let digest = derive_delta_digest(&files).expect("delta digest");
        let store =
            WorkspaceTreeStore::open(&fixture.checkout, &fixture.state).expect("tree store");
        let candidate = store
            .compute_tree(&fixture.source, &files, &digest)
            .expect("candidate tree");
        let before = fs::read(fixture.checkout.join("update.txt")).expect("candidate bytes");
        let mut journal = MemoryJournal {
            intents: Vec::new(),
            fail: true,
        };
        assert_eq!(
            store
                .restore_tree("workspace-1", &candidate, &fixture.source, &mut journal)
                .expect_err("journal failure")
                .code(),
            WorkspaceTreeErrorCode::Journal
        );
        assert_eq!(
            fs::read(fixture.checkout.join("update.txt")).expect("unchanged"),
            before
        );
    }

    #[test]
    fn restore_intent_identity_includes_both_expected_and_target_trees() {
        let expected = WorkspaceRevision(format!("git-tree:{}", "a".repeat(40)));
        let first_target = WorkspaceRevision(format!("git-tree:{}", "b".repeat(40)));
        let second_target = WorkspaceRevision(format!("git-tree:{}", "c".repeat(40)));
        let mut journal = MemoryJournal {
            intents: Vec::new(),
            fail: false,
        };
        let first = WorkspaceTreeRestoreIntent {
            workspace_id: "workspace-1".to_owned(),
            expected_current: expected.clone(),
            target: first_target,
        };
        let second = WorkspaceTreeRestoreIntent {
            workspace_id: "workspace-1".to_owned(),
            expected_current: expected,
            target: second_target,
        };

        journal
            .persist_restore_intent_and_sync(&first)
            .expect("first intent");
        journal
            .persist_restore_intent_and_sync(&first)
            .expect("exact replay");
        journal
            .persist_restore_intent_and_sync(&second)
            .expect("same expected tree with a later target");

        assert_eq!(journal.intents, vec![first, second]);
    }

    #[test]
    fn restore_preserves_extras_ignored_by_the_expected_current_tree() {
        let fixture = fixture("sha1");
        apply_four_operations(&fixture.checkout);
        write(
            &fixture.checkout.join(".gitignore"),
            b"*.ignored\n*.tmp\n",
            REGULAR_MODE,
        );
        write(
            &fixture.checkout.join("preserve.tmp"),
            b"ignored by candidate\n",
            REGULAR_MODE,
        );
        let mut files = four_operation_delta();
        files.push(summary(
            ".gitignore",
            AppliedFileOperation::Update,
            None,
            Some(b"*.ignored\n"),
            Some(b"*.ignored\n*.tmp\n"),
            Some("0644"),
            Some("0644"),
        ));
        let digest = derive_delta_digest(&files).expect("delta digest");
        let store =
            WorkspaceTreeStore::open(&fixture.checkout, &fixture.state).expect("tree store");
        let candidate = store
            .compute_tree(&fixture.source, &files, &digest)
            .expect("candidate with changed ignore rules");
        let mut journal = MemoryJournal {
            intents: Vec::new(),
            fail: false,
        };

        assert_eq!(
            store
                .restore_tree("workspace-1", &candidate, &fixture.source, &mut journal)
                .expect("restore changed ignore rules"),
            WorkspaceTreeRestoreOutcome::ExactRestored
        );
        assert_eq!(
            fs::read(fixture.checkout.join(".gitignore")).expect("accepted ignore rules"),
            b"*.ignored\n"
        );
        assert_eq!(
            fs::read(fixture.checkout.join("preserve.tmp")).expect("preserved ignored extra"),
            b"ignored by candidate\n"
        );
    }

    #[test]
    fn poisoned_git_environment_is_explicitly_removed() {
        let fixture = fixture("sha1");
        for key in [
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_DIR",
            "GIT_WORK_TREE",
        ] {
            assert!(is_git_environment_key(OsStr::new(key)));
        }

        let store =
            WorkspaceTreeStore::open(&fixture.checkout, &fixture.state).expect("tree store");
        let private = GitEnvironment::private(&store).command();
        let private_environment: BTreeMap<_, _> = private
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
            .collect();
        assert_eq!(
            private_environment.get(OsStr::new("GIT_INDEX_FILE")),
            Some(&store.private_index.as_os_str().to_owned())
        );
        assert_eq!(
            private_environment.get(OsStr::new("GIT_OBJECT_DIRECTORY")),
            Some(&store.private_objects.as_os_str().to_owned())
        );
    }

    #[test]
    fn sha256_repository_is_supported_when_local_git_supports_it() {
        let probe = tempfile::tempdir().expect("SHA-256 probe root");
        let supported = Command::new("git")
            .current_dir(probe.path())
            .args(["init", "--object-format=sha256"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !supported {
            return;
        }
        let fixture = fixture("sha256");
        let store =
            WorkspaceTreeStore::open(&fixture.checkout, &fixture.state).expect("tree store");
        assert_eq!(fixture.source.0.len(), "git-tree:".len() + 64);
        assert_eq!(
            store
                .compare_tree(&fixture.source)
                .expect("SHA-256 comparison"),
            WorkspaceTreeComparison::Exact
        );
    }

    #[test]
    fn planner_portable_path_rules_are_rechecked_for_every_enumerated_file() {
        for valid in ["src/lib.rs", "docs/设计.md", "a-b_c.txt"] {
            assert!(validate_path(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "../escape",
            "/absolute",
            "double//slash",
            "back\\slash",
            ".git/config",
            "CON",
            "aux.txt",
            "COM1.log",
            "trailing.",
            "trailing ",
            "colon:name",
            "wild*card",
        ] {
            assert!(validate_path(invalid).is_err(), "{invalid}");
        }
    }
}
