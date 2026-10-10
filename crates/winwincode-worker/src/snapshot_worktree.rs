use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub struct SnapshotWorktreeError {
    code: SnapshotWorktreeErrorCode,
    message: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SnapshotWorktreeErrorCode {
    InvalidInput,
    Git,
    Io,
}

impl SnapshotWorktreeError {
    fn new(code: SnapshotWorktreeErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn code(&self) -> SnapshotWorktreeErrorCode {
        self.code
    }
}

impl std::fmt::Display for SnapshotWorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SnapshotWorktreeError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenWorktree {
    pub path: PathBuf,
    pub commit_id: String,
    pub tree_id: String,
}

pub fn freeze_worktree(
    repo: &Path,
    candidate_commit: &str,
    dest: &Path,
) -> Result<FrozenWorktree, SnapshotWorktreeError> {
    if !repo.is_dir() {
        return Err(SnapshotWorktreeError::new(
            SnapshotWorktreeErrorCode::InvalidInput,
            "snapshot source repository is missing",
        ));
    }
    if dest.exists() {
        return Err(SnapshotWorktreeError::new(
            SnapshotWorktreeErrorCode::InvalidInput,
            "snapshot destination already exists",
        ));
    }
    let resolved = git(
        repo,
        &[
            "rev-parse",
            "--verify",
            &format!("{candidate_commit}^{{commit}}"),
        ],
    )?;
    if resolved != candidate_commit {
        return Err(SnapshotWorktreeError::new(
            SnapshotWorktreeErrorCode::InvalidInput,
            "candidate commit does not resolve to the requested identity",
        ));
    }
    let tree_id = git(
        repo,
        &["rev-parse", &format!("{candidate_commit}^{{tree}}")],
    )?;
    git(
        repo,
        &[
            "worktree",
            "add",
            "--detach",
            &dest.to_string_lossy(),
            candidate_commit,
        ],
    )?;
    Ok(FrozenWorktree {
        path: dest.to_path_buf(),
        commit_id: candidate_commit.to_owned(),
        tree_id,
    })
}

pub fn drop_worktree(repo: &Path, dest: &Path) -> Result<(), SnapshotWorktreeError> {
    git(
        repo,
        &["worktree", "remove", "--force", &dest.to_string_lossy()],
    )?;
    Ok(())
}

fn git(repo: &Path, args: &[&str]) -> Result<String, SnapshotWorktreeError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .map_err(|error| {
            SnapshotWorktreeError::new(
                SnapshotWorktreeErrorCode::Io,
                format!("git is unavailable: {error}"),
            )
        })?;
    if !out.status.success() {
        return Err(SnapshotWorktreeError::new(
            SnapshotWorktreeErrorCode::Git,
            format!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
