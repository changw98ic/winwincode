// SPDX-License-Identifier: Apache-2.0

//! The independent Git inspector behind the repository registry
//! (plan 13.2 steps 3–5, deepened from the registry lane).
//!
//! [`GitInspector`] owns every Git fact the registration check chain
//! ([`crate::repository`]) and the launch-time revalidation need: whether a
//! directory is a Git repository at all, the Git common directory, the branch
//! or detached-HEAD label, the HEAD commit (including the unborn-branch
//! case), and the working-tree dirty projection. Extracting the probes here
//! keeps the Git surface independently exercisable against the repository
//! shapes it must classify stably:
//!
//! | repository shape | inspector classification |
//! |---|---|
//! | plain repository, attached HEAD | healthy scan, [`GitHeadState::Attached`] |
//! | dirty working tree | healthy scan, `dirty` projection |
//! | fresh `git init`, no commit yet | healthy scan, [`GitHeadState::Unborn`], empty HEAD |
//! | detached checkout | healthy scan, branch label [`DETACHED_BRANCH`], [`GitHeadState::Detached`] |
//! | linked worktree (`git worktree add`) | healthy scan; common directory is the main repository's |
//! | submodule working directory | healthy scan; common directory is the superproject's `modules/<name>` |
//! | bare repository | refused, `invalid_git` (Git data without a working tree to bind) |
//! | not a Git repository | refused, `invalid_git` unless a confirmed init ran |
//! | `git` binary missing or unusable | refused, `scan_failed` |
//!
//! The seven-state projection of every failure rides
//! [`GitInspectError::availability`], so the registry maps an inspection
//! failure onto the plan 13.5 vocabulary with one call. A repository without
//! a remote origin inspects exactly like any other: remotes are never probed
//! and GitHub is never required (plan 13.3).
//!
//! Local-data boundary: inspection is read-only unless
//! [`GitInspectOptions::allow_git_init`] runs a confirmed `git init`. The
//! only paths touched are the inspected directory chain (for canonicalization
//! done by the caller) and the Git-reported common directory (a metadata
//! resolution, which for linked worktrees and submodules legitimately lives
//! in the owning repository). Remotes are never contacted.
//!
//! Git interconnect: every probe shells out to the system `git` binary
//! through [`Command`] — the same dependency-free convention the registry
//! landed with; no new crate dependency is introduced.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use winwincode_client_port::domain::{RepositoryAvailability, RepositoryDirtyState};

/// Branch label reported for a detached HEAD, where no symbolic branch name
/// exists. Git refuses to create a branch literally named `HEAD`
/// (`git check-ref-format`), so the label is unambiguous.
pub const DETACHED_BRANCH: &str = "HEAD";

const OPERATION_DETECT: &str = "detect the Git repository";
const OPERATION_COMMON_DIRECTORY: &str = "read the Git common directory";
const OPERATION_BARE: &str = "classify the repository";
const OPERATION_BRANCH: &str = "read the current branch";
const OPERATION_HEAD: &str = "read HEAD";
const OPERATION_STATUS: &str = "read the working-tree status";

/// The HEAD shape one inspection resolved, for launch policies that must
/// treat a detached checkout or an unborn branch differently from an
/// attached one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitHeadState {
    /// HEAD points at a commit through a branch.
    Attached,
    /// HEAD points at a commit directly (branch label [`DETACHED_BRANCH`]).
    Detached,
    /// The branch exists but has no commit yet (fresh `git init`); the scan
    /// reports an empty HEAD.
    Unborn,
}

/// The Git facts one successful inspection observed about one
/// already-canonical directory. Absolute paths stay local-only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitScan {
    /// Absolute, canonicalized Git common directory: `<root>/.git` for a
    /// plain repository, the main repository's directory for a linked
    /// worktree, and the superproject's `.git/modules/<name>` for a
    /// submodule working directory.
    pub git_common_directory: PathBuf,
    /// Short branch name, or the [`DETACHED_BRANCH`] label on a detached
    /// checkout; never empty on a successful scan.
    pub branch: String,
    /// HEAD commit (lowercase hex), or empty for an unborn branch.
    pub head_commit: String,
    /// Working-tree dirty projection: any `git status --porcelain` row
    /// (staged, unstaged, or untracked) means dirty.
    pub dirty_state: RepositoryDirtyState,
    /// The resolved HEAD shape.
    pub head_state: GitHeadState,
    /// Whether this inspection ran a confirmed `git init`.
    pub initialized_by_inspection: bool,
}

/// Options for one inspection. `git init` never runs without
/// [`GitInspectOptions::allow_git_init`] (plan 13.3: an explicit user
/// confirmation must precede initialization).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GitInspectOptions {
    /// Whether a non-Git directory may be initialized by this inspection.
    /// The default is `false`: inspection is read-only unless the caller
    /// carries an explicit user confirmation.
    pub allow_git_init: bool,
}

/// The stateless Git probing surface. Every call reads one directory's Git
/// state from scratch; no result is cached between inspections, so callers
/// (the registry's launch-time revalidation) always observe the current
/// repository shape.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitInspector;

impl GitInspector {
    /// Creates the inspector.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Inspects one already-canonical directory (the registry check chain
    /// canonicalizes before calling). Read-only unless
    /// [`GitInspectOptions::allow_git_init`] is set.
    ///
    /// # Errors
    ///
    /// Returns [`GitInspectError`] for every refused shape; the seven-state
    /// projection of the refusal rides [`GitInspectError::availability`] and
    /// the local-only human detail rides [`GitInspectError::detail`]. A
    /// refusal to classify is a value, not a panic: callers decide whether a
    /// refused registration or a failed launch matters.
    pub fn inspect(
        &self,
        canonical_root: &Path,
        options: &GitInspectOptions,
    ) -> Result<GitScan, GitInspectError> {
        let mut initialized_by_inspection = false;
        match probe_git(canonical_root, &["rev-parse", "--git-dir"]) {
            Ok(_) => {}
            Err(ProbeFailure::Unavailable(source)) => {
                return Err(GitInspectError::GitUnavailable {
                    operation: OPERATION_DETECT,
                    source,
                });
            }
            Err(ProbeFailure::Failed(_)) if options.allow_git_init => {
                // A refused inspection never mutates the directory: `git
                // init` runs only on the explicit confirmation.
                initialize_git(canonical_root)?;
                initialized_by_inspection = true;
            }
            Err(ProbeFailure::Failed(_)) => {
                return Err(GitInspectError::NotARepository {
                    root: root_display(canonical_root),
                });
            }
        }

        // A bare repository answers every probe but has no working tree to
        // bind or to scan for dirt; classify it deliberately instead of
        // failing later, mid-scan, on the status read.
        if is_bare_repository(canonical_root)? {
            return Err(GitInspectError::BareRepository {
                root: root_display(canonical_root),
            });
        }

        let git_common_directory = common_directory(canonical_root)?;
        let branch = read_branch(canonical_root)?;
        let head_commit = read_head(canonical_root, &branch)?;
        let dirty_state = read_dirty_state(canonical_root)?;
        let head_state = if branch == DETACHED_BRANCH {
            GitHeadState::Detached
        } else if head_commit.is_empty() {
            GitHeadState::Unborn
        } else {
            GitHeadState::Attached
        };
        Ok(GitScan {
            git_common_directory,
            branch,
            head_commit,
            dirty_state,
            head_state,
            initialized_by_inspection,
        })
    }
}

/// Why one inspection refused a directory.
#[derive(Debug)]
pub enum GitInspectError {
    /// The `git` binary is missing or could not be spawned.
    GitUnavailable {
        /// The probe operation that failed.
        operation: &'static str,
        /// The spawn failure.
        source: io::Error,
    },
    /// The directory is not a Git repository and no confirmed init ran.
    NotARepository {
        /// Local-only display of the inspected directory.
        root: String,
    },
    /// The directory is a bare Git repository: Git data without a working
    /// tree, which cannot be bound as an editable repository.
    BareRepository {
        /// Local-only display of the inspected directory.
        root: String,
    },
    /// Git ran and refused an expected read (common directory, working-tree
    /// status, repository classification).
    GitRefused {
        /// The local-only failure detail.
        detail: String,
    },
    /// `git symbolic-ref` answered with an empty branch name.
    EmptyBranchName,
    /// HEAD could not be resolved on a detached checkout: a corrupt
    /// repository shape (an unborn branch resolves through its branch and
    /// never reaches this state).
    DetachedHeadUnreadable {
        /// Git's own failure detail.
        detail: String,
    },
    /// A confirmed `git init` was attempted and failed.
    InitFailed {
        /// The local-only failure detail.
        detail: String,
    },
}

impl GitInspectError {
    /// The plan 13.5 availability state this refusal maps to. `invalid_git`
    /// covers shapes that are not a bindable working repository (including
    /// bare repositories); everything else is a `scan_failed` fact about a
    /// directory that may still be a valid repository.
    #[must_use]
    pub const fn availability(&self) -> RepositoryAvailability {
        match self {
            Self::NotARepository { .. } | Self::BareRepository { .. } => {
                RepositoryAvailability::InvalidGit
            }
            Self::GitUnavailable { .. }
            | Self::GitRefused { .. }
            | Self::EmptyBranchName
            | Self::DetachedHeadUnreadable { .. }
            | Self::InitFailed { .. } => RepositoryAvailability::ScanFailed,
        }
    }

    /// The local-only human-readable detail. Absolute paths are allowed
    /// here; this string never reaches a server-bound frame.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::GitUnavailable { operation, source } => {
                format!("{operation}: git is not usable: {source}")
            }
            Self::NotARepository { root } => format!(
                "the directory is not a Git repository: {root}; pass the explicit \
                 confirmation to initialize one"
            ),
            Self::BareRepository { root } => {
                format!("the directory is a bare Git repository without a working tree: {root}")
            }
            // Both variants carry the final, preformatted local-only detail.
            Self::GitRefused { detail } | Self::InitFailed { detail } => detail.clone(),
            Self::DetachedHeadUnreadable { detail } => {
                format!("HEAD is unreadable on a detached checkout: {detail}")
            }
            Self::EmptyBranchName => "git reported an empty branch name".to_owned(),
        }
    }
}

impl fmt::Display for GitInspectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "git inspection failed: {}", self.detail())
    }
}

impl Error for GitInspectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::GitUnavailable { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The `sha256:` repository fingerprint reported as `repositoryFingerprint`.
///
/// # Rule (frozen by the registry lane, documented and pinned here)
///
/// ```text
/// fingerprint = "sha256:" ++ lowercase_hex(sha256(head_commit_utf8 ++ 0x00 ++ branch_utf8))
/// ```
///
/// - `head_commit` is the scan's HEAD: the 40 (SHA-1) or 64 (NewHash)
///   lowercase hex characters of `git rev-parse --verify HEAD`, or the empty
///   string when the branch is unborn or a failed revalidation could not
///   read a HEAD.
/// - `branch` is the short branch name from `git symbolic-ref --short HEAD`,
///   or the [`DETACHED_BRANCH`] label on a detached checkout. Git refnames
///   follow `git check-ref-format` rules: no NUL byte, no space, no ASCII
///   control characters, no `..` or `@{` sequences — so a contract branch
///   can never contain the separator byte.
/// - The `0x00` separator makes the byte concatenation unambiguous: within
///   the contract inputs (hex-or-empty head, refname-or-label branch) no
///   input can contain NUL, so distinct `(head, branch)` pairs always hash
///   differently.
/// - `("", "")` is the reserved "the scan could not read an identity" value
///   used by failed revalidations; a healthy scan always reports a non-empty
///   branch, so the sentinel never collides with a real repository scan.
/// - The output is `sha256:` plus 64 lowercase hex digits — 71 printable
///   ASCII bytes, within the Server's 128-byte printable-ASCII fingerprint
///   bound.
///
/// # What it binds — and what it does not
///
/// The fingerprint binds *which commit and branch a scan observed*, nothing
/// more. It deliberately excludes the repository location, the Git common
/// directory, the dirty state, and any remote name: two bindings observing
/// the same commit and branch share a fingerprint by design, and a local
/// path never enters a server-visible value. Branch names are case-sensitive
/// byte strings. It is an unkeyed identity digest, not an integrity proof or
/// a security boundary; it must not be used to authenticate repository
/// contents.
#[must_use]
pub fn repository_fingerprint(head_commit: &str, branch: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(head_commit.as_bytes());
    digest.update([0_u8]);
    digest.update(branch.as_bytes());
    format!("sha256:{:x}", digest.finalize())
}

/// Why one `git` invocation could not answer.
enum ProbeFailure {
    /// The `git` binary itself is missing or unusable.
    Unavailable(io::Error),
    /// Git ran and rejected the operation.
    Failed(String),
}

fn probe_git(root: &Path, arguments: &[&str]) -> Result<String, ProbeFailure> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .map_err(ProbeFailure::Unavailable)?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(ProbeFailure::Failed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

fn initialize_git(root: &Path) -> Result<(), GitInspectError> {
    let output = Command::new("git").arg("-C").arg(root).arg("init").output();
    let output = output.map_err(|source| GitInspectError::InitFailed {
        detail: format!("git is not usable: {source}"),
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(GitInspectError::InitFailed {
            detail: format!(
                "git init failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        })
    }
}

/// A bare repository has Git data but no working tree: refuse it before any
/// working-tree probe runs.
fn is_bare_repository(root: &Path) -> Result<bool, GitInspectError> {
    match probe_git(root, &["rev-parse", "--is-bare-repository"]) {
        Ok(answer) => Ok(answer == "true"),
        Err(ProbeFailure::Unavailable(source)) => Err(GitInspectError::GitUnavailable {
            operation: OPERATION_BARE,
            source,
        }),
        Err(ProbeFailure::Failed(stderr)) => Err(GitInspectError::GitRefused {
            detail: format!("{OPERATION_BARE}: {stderr}"),
        }),
    }
}

/// Resolves the absolute Git common directory (plan 13.2 step 4). Plain
/// repositories answer with a relative `.git`; linked worktrees and
/// submodule working directories answer with the owning repository's
/// absolute common dir. The result is canonicalized so the stored value is
/// stable across the symlink spelling of the day.
fn common_directory(root: &Path) -> Result<PathBuf, GitInspectError> {
    let reported = match probe_git(root, &["rev-parse", "--git-common-dir"]) {
        Ok(reported) => reported,
        Err(ProbeFailure::Unavailable(source)) => {
            return Err(GitInspectError::GitUnavailable {
                operation: OPERATION_COMMON_DIRECTORY,
                source,
            });
        }
        Err(ProbeFailure::Failed(stderr)) => {
            return Err(GitInspectError::GitRefused {
                detail: format!(
                    "the Git common directory is unreadable: {OPERATION_COMMON_DIRECTORY}: {stderr}"
                ),
            });
        }
    };
    let reported_path = PathBuf::from(&reported);
    let joined = if reported_path.is_absolute() {
        reported_path
    } else {
        root.join(reported_path)
    };
    fs::canonicalize(&joined).map_err(|error| GitInspectError::GitRefused {
        detail: format!(
            "the Git common directory cannot be resolved: {} ({error})",
            joined.to_string_lossy()
        ),
    })
}

/// Reads the current branch. A detached HEAD reports the [`DETACHED_BRANCH`]
/// label; an unborn branch (fresh `git init`, no commit yet) still reports
/// its name.
fn read_branch(root: &Path) -> Result<String, GitInspectError> {
    match probe_git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        Ok(branch) if !branch.is_empty() => Ok(branch),
        Ok(_) => Err(GitInspectError::EmptyBranchName),
        Err(ProbeFailure::Unavailable(source)) => Err(GitInspectError::GitUnavailable {
            operation: OPERATION_BRANCH,
            source,
        }),
        Err(ProbeFailure::Failed(_)) => Ok(DETACHED_BRANCH.to_owned()),
    }
}

/// Reads HEAD. An unborn branch has no commit: the empty HEAD is a healthy
/// scan fact, not a failure. HEAD unreadable on a detached checkout means a
/// corrupt repository and maps to `scan_failed`.
fn read_head(root: &Path, branch: &str) -> Result<String, GitInspectError> {
    match probe_git(root, &["rev-parse", "--verify", "HEAD"]) {
        Ok(head) => Ok(head),
        Err(ProbeFailure::Unavailable(source)) => Err(GitInspectError::GitUnavailable {
            operation: OPERATION_HEAD,
            source,
        }),
        Err(ProbeFailure::Failed(detail)) => {
            if branch == DETACHED_BRANCH {
                Err(GitInspectError::DetachedHeadUnreadable { detail })
            } else {
                Ok(String::new())
            }
        }
    }
}

/// Reads the dirty projection: any `git status --porcelain` row (staged,
/// unstaged, or untracked) means dirty.
fn read_dirty_state(root: &Path) -> Result<RepositoryDirtyState, GitInspectError> {
    match probe_git(root, &["status", "--porcelain"]) {
        Ok(status) => Ok(if status.is_empty() {
            RepositoryDirtyState::Clean
        } else {
            RepositoryDirtyState::Dirty
        }),
        Err(ProbeFailure::Unavailable(source)) => Err(GitInspectError::GitUnavailable {
            operation: OPERATION_STATUS,
            source,
        }),
        Err(ProbeFailure::Failed(stderr)) => Err(GitInspectError::GitRefused {
            detail: format!("the working-tree status is unreadable: {OPERATION_STATUS}: {stderr}"),
        }),
    }
}

/// The local-only display of an inspected root.
fn root_display(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMPORARY_BASE: AtomicU64 = AtomicU64::new(1);

    /// One canonical temporary base directory (canonicalized because the
    /// platform temp path itself may be a symlink spelling).
    fn temporary_base(name: &str) -> PathBuf {
        let suffix = NEXT_TEMPORARY_BASE.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "winwincode-git-inspector-{name}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&base).expect("temporary base directory");
        fs::canonicalize(&base).expect("canonical temporary base")
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    /// Fails loudly unless the system git is on PATH: every scenario shells
    /// out to real Git, so a missing binary must fail the suite, not skip it.
    fn require_git() {
        let available = Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        assert!(available, "system git must be available on PATH");
    }

    /// Runs one git command with an isolated configuration and fails on
    /// error.
    fn git(root: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// One real temporary Git repository with a baseline commit.
    fn fresh_repository(base: &Path, name: &str) -> PathBuf {
        require_git();
        let root = base.join(name);
        fs::create_dir_all(&root).expect("repository directory");
        git(&root, &["init"]);
        git(&root, &["config", "user.email", "inspector@example.test"]);
        git(&root, &["config", "user.name", "Git Inspector Tests"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        git(&root, &["commit", "--allow-empty", "-m", "baseline"]);
        root
    }

    fn inspect(root: &Path) -> Result<GitScan, GitInspectError> {
        GitInspector::new().inspect(root, &GitInspectOptions::default())
    }

    #[test]
    fn plain_repository_scans_attached_clean_and_common_directory() {
        let base = temporary_base("plain");
        let root = fresh_repository(&base, "plain-repo");

        let scan = inspect(&root).expect("plain repository inspects");
        assert_eq!(
            scan.branch,
            git(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        );
        assert_eq!(scan.head_commit, git(&root, &["rev-parse", "HEAD"]));
        assert_eq!(scan.head_state, GitHeadState::Attached);
        assert_eq!(scan.dirty_state, RepositoryDirtyState::Clean);
        assert_eq!(
            scan.git_common_directory,
            fs::canonicalize(root.join(".git")).expect("common dir")
        );
        assert!(!scan.initialized_by_inspection);

        cleanup(&base);
    }

    #[test]
    fn dirty_working_tree_reports_dirty_projection() {
        let base = temporary_base("dirty");
        let root = fresh_repository(&base, "dirty-repo");
        fs::write(root.join("untracked.txt"), "local change".as_bytes()).expect("dirty file");

        let scan = inspect(&root).expect("dirty repository inspects");
        assert_eq!(scan.dirty_state, RepositoryDirtyState::Dirty);
        assert_eq!(scan.head_state, GitHeadState::Attached);

        cleanup(&base);
    }

    #[test]
    fn unborn_branch_is_a_healthy_scan_with_empty_head() {
        let base = temporary_base("unborn");
        require_git();
        let root = base.join("unborn-repo");
        fs::create_dir_all(&root).expect("repository directory");
        git(&root, &["init"]);

        let scan = inspect(&root).expect("unborn repository inspects");
        assert_eq!(
            scan.branch,
            git(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        );
        assert!(!scan.branch.is_empty());
        assert_eq!(scan.head_commit, "");
        assert_eq!(scan.head_state, GitHeadState::Unborn);
        assert_eq!(scan.dirty_state, RepositoryDirtyState::Clean);

        cleanup(&base);
    }

    #[test]
    fn detached_checkout_reports_the_head_label() {
        let base = temporary_base("detached");
        let root = fresh_repository(&base, "detached-repo");
        git(&root, &["checkout", "--detach", "HEAD"]);

        let scan = inspect(&root).expect("detached repository inspects");
        assert_eq!(scan.branch, DETACHED_BRANCH);
        assert_eq!(scan.head_commit, git(&root, &["rev-parse", "HEAD"]));
        assert_eq!(scan.head_state, GitHeadState::Detached);
        assert_eq!(scan.dirty_state, RepositoryDirtyState::Clean);

        cleanup(&base);
    }

    #[test]
    fn linked_worktree_reports_the_main_common_directory() {
        let base = temporary_base("worktree");
        let main = fresh_repository(&base, "main-repo");
        let worktree = base.join("linked-worktree");
        let worktree_text = worktree.to_string_lossy().into_owned();
        git(
            &main,
            &["worktree", "add", "-b", "wt-branch", worktree_text.as_str()],
        );

        let scan = inspect(&worktree).expect("linked worktree inspects");
        assert_eq!(scan.branch, "wt-branch");
        assert_eq!(scan.head_state, GitHeadState::Attached);
        assert_eq!(
            scan.git_common_directory,
            fs::canonicalize(main.join(".git")).expect("main common dir")
        );

        cleanup(&base);
    }

    #[test]
    fn submodule_working_directory_reports_the_module_common_directory() {
        let base = temporary_base("submodule");
        let inner = fresh_repository(&base, "inner-library");
        let super_root = fresh_repository(&base, "super-project");
        let inner_text = inner.to_string_lossy().into_owned();
        git(
            &super_root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                inner_text.as_str(),
                "sub",
            ],
        );
        let submodule = super_root.join("sub");

        let scan = inspect(&submodule).expect("submodule working directory inspects");
        assert_eq!(
            scan.branch,
            git(&submodule, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        );
        assert_eq!(scan.head_commit, git(&submodule, &["rev-parse", "HEAD"]));
        assert_eq!(
            scan.git_common_directory,
            fs::canonicalize(super_root.join(".git").join("modules").join("sub"))
                .expect("module common dir")
        );

        cleanup(&base);
    }

    #[test]
    fn bare_repository_is_classified_invalid_git() {
        let base = temporary_base("bare");
        require_git();
        let bare_root = base.join("bare-repo");
        fs::create_dir_all(&bare_root).expect("bare directory");
        git(&bare_root, &["init", "--bare"]);

        let error = inspect(&bare_root).expect_err("bare repository is refused");
        assert!(matches!(error, GitInspectError::BareRepository { .. }));
        assert_eq!(error.availability(), RepositoryAvailability::InvalidGit);
        assert!(error.detail().contains("bare"));

        cleanup(&base);
    }

    #[test]
    fn non_git_directory_is_refused_and_left_untouched() {
        let base = temporary_base("non-git");
        let plain = base.join("plain-directory");
        fs::create_dir_all(&plain).expect("plain directory");

        let error = inspect(&plain).expect_err("non-Git directory is refused");
        assert!(matches!(error, GitInspectError::NotARepository { .. }));
        assert_eq!(error.availability(), RepositoryAvailability::InvalidGit);
        assert!(error.detail().contains("explicit confirmation"));
        assert!(
            !plain.join(".git").exists(),
            "a refused inspection never mutates"
        );

        cleanup(&base);
    }

    #[test]
    fn confirmed_init_initializes_and_reports_unborn() {
        let base = temporary_base("confirmed-init");
        let plain = base.join("init-directory");
        fs::create_dir_all(&plain).expect("plain directory");

        let scan = GitInspector::new()
            .inspect(
                &plain,
                &GitInspectOptions {
                    allow_git_init: true,
                },
            )
            .expect("confirmed init runs");
        assert!(scan.initialized_by_inspection);
        assert!(plain.join(".git").exists(), "git init ran");
        assert_eq!(scan.head_commit, "");
        assert_eq!(scan.head_state, GitHeadState::Unborn);

        cleanup(&base);
    }

    #[test]
    fn every_refusal_maps_to_a_stable_availability_state() {
        let cases: Vec<(GitInspectError, RepositoryAvailability)> = vec![
            (
                GitInspectError::GitUnavailable {
                    operation: OPERATION_DETECT,
                    source: io::Error::new(io::ErrorKind::NotFound, "no git"),
                },
                RepositoryAvailability::ScanFailed,
            ),
            (
                GitInspectError::NotARepository {
                    root: "/tmp/plain".to_owned(),
                },
                RepositoryAvailability::InvalidGit,
            ),
            (
                GitInspectError::BareRepository {
                    root: "/tmp/bare".to_owned(),
                },
                RepositoryAvailability::InvalidGit,
            ),
            (
                GitInspectError::GitRefused {
                    detail: "unreadable".to_owned(),
                },
                RepositoryAvailability::ScanFailed,
            ),
            (
                GitInspectError::EmptyBranchName,
                RepositoryAvailability::ScanFailed,
            ),
            (
                GitInspectError::DetachedHeadUnreadable {
                    detail: "corrupt".to_owned(),
                },
                RepositoryAvailability::ScanFailed,
            ),
            (
                GitInspectError::InitFailed {
                    detail: "failed".to_owned(),
                },
                RepositoryAvailability::ScanFailed,
            ),
        ];
        for (error, availability) in cases {
            assert_eq!(error.availability(), availability, "{error}");
            assert!(!error.detail().is_empty(), "{error}");
        }
    }

    #[test]
    fn fingerprint_rule_pins_the_exact_byte_recipe() {
        // The frozen recipe: sha256(head_utf8 ++ 0x00 ++ branch_utf8).
        let mut expected = Sha256::new();
        expected.update(b"abc");
        expected.update([0_u8]);
        expected.update(b"main");
        assert_eq!(
            repository_fingerprint("abc", "main"),
            format!("sha256:{:x}", expected.finalize())
        );
    }

    #[test]
    fn fingerprint_edge_cases_are_stable_and_distinct() {
        let base = repository_fingerprint("abc", "main");

        // Format: "sha256:" plus 64 lowercase hex digits (71 printable ASCII
        // bytes, within the Server's 128-byte bound).
        assert!(base.starts_with("sha256:"));
        let digest = base.trim_start_matches("sha256:");
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| matches!(
            byte,
            b'0'..=b'9' | b'a'..=b'f'
        )));

        // NewHash-length (64 hex character) heads fingerprint the same way.
        let new_hash_head = "a".repeat(64);
        assert_eq!(
            repository_fingerprint(&new_hash_head, "main"),
            repository_fingerprint(&new_hash_head, "main")
        );
        assert_ne!(base, repository_fingerprint(&new_hash_head, "main"));

        // Branch names are case-sensitive byte strings.
        assert_ne!(base, repository_fingerprint("abc", "Main"));

        // Unicode branch names hash stably as UTF-8.
        let unicode = repository_fingerprint("abc", "feature/日本語");
        assert_eq!(unicode, repository_fingerprint("abc", "feature/日本語"));
        assert_ne!(unicode, repository_fingerprint("abc", "feature"));

        // The detached label is a distinct identity from any attached pair.
        assert_ne!(
            repository_fingerprint("abc", DETACHED_BRANCH),
            repository_fingerprint("", "abc")
        );
        assert_ne!(repository_fingerprint("abc", DETACHED_BRANCH), base);

        // ("", "") is the reserved failed-scan sentinel: distinct from every
        // pair a healthy scan can produce (a healthy branch is never empty).
        let sentinel = repository_fingerprint("", "");
        assert_ne!(sentinel, base);
        assert_ne!(sentinel, repository_fingerprint("", "main"));
        assert_ne!(sentinel, repository_fingerprint("abc", ""));
    }
}
