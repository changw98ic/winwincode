use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use winwincode_client_port::domain::{RepositoryAvailability, RepositoryDirtyState};

pub const DETACHED_BRANCH: &str = "HEAD";

const OPERATION_DETECT: &str = "detect the Git repository";
const OPERATION_COMMON_DIRECTORY: &str = "read the Git common directory";
const OPERATION_BARE: &str = "classify the repository";
const OPERATION_BRANCH: &str = "read the current branch";
const OPERATION_HEAD: &str = "read HEAD";
const OPERATION_STATUS: &str = "read the working-tree status";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitHeadState {
    Attached,
    Detached,
    Unborn,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitScan {
    pub git_common_directory: PathBuf,
    pub branch: String,
    pub head_commit: String,
    pub dirty_state: RepositoryDirtyState,
    pub head_state: GitHeadState,
    pub initialized_by_inspection: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GitInspectOptions {
    pub allow_git_init: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GitInspector;

impl GitInspector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

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
                initialize_git(canonical_root)?;
                initialized_by_inspection = true;
            }
            Err(ProbeFailure::Failed(_)) => {
                return Err(GitInspectError::NotARepository {
                    root: root_display(canonical_root),
                });
            }
        }

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

#[derive(Debug)]
pub enum GitInspectError {
    GitUnavailable {
        operation: &'static str,
        source: io::Error,
    },
    NotARepository {
        root: String,
    },
    BareRepository {
        root: String,
    },
    GitRefused {
        detail: String,
    },
    EmptyBranchName,
    DetachedHeadUnreadable {
        detail: String,
    },
    InitFailed {
        detail: String,
    },
}

impl GitInspectError {
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

#[must_use]
pub fn repository_fingerprint(head_commit: &str, branch: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(head_commit.as_bytes());
    digest.update([0_u8]);
    digest.update(branch.as_bytes());
    format!("sha256:{:x}", digest.finalize())
}

enum ProbeFailure {
    Unavailable(io::Error),
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

fn root_display(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMPORARY_BASE: AtomicU64 = AtomicU64::new(1);

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

    fn require_git() {
        let available = Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        assert!(available, "system git must be available on PATH");
    }

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

        assert!(base.starts_with("sha256:"));
        let digest = base.trim_start_matches("sha256:");
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| matches!(
            byte,
            b'0'..=b'9' | b'a'..=b'f'
        )));

        let new_hash_head = "a".repeat(64);
        assert_eq!(
            repository_fingerprint(&new_hash_head, "main"),
            repository_fingerprint(&new_hash_head, "main")
        );
        assert_ne!(base, repository_fingerprint(&new_hash_head, "main"));

        assert_ne!(base, repository_fingerprint("abc", "Main"));

        let unicode = repository_fingerprint("abc", "feature/日本語");
        assert_eq!(unicode, repository_fingerprint("abc", "feature/日本語"));
        assert_ne!(unicode, repository_fingerprint("abc", "feature"));

        assert_ne!(
            repository_fingerprint("abc", DETACHED_BRANCH),
            repository_fingerprint("", "abc")
        );
        assert_ne!(repository_fingerprint("abc", DETACHED_BRANCH), base);

        let sentinel = repository_fingerprint("", "");
        assert_ne!(sentinel, base);
        assert_ne!(sentinel, repository_fingerprint("", "main"));
        assert_ne!(sentinel, repository_fingerprint("abc", ""));
    }
}
