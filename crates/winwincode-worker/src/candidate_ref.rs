// SPDX-License-Identifier: Apache-2.0

//! Stable Candidate Git refs published before Worktree cleanup.
//!
//! After the Worker freezes checkout changes into a deterministic candidate
//! commit, this module records `refs/winwincode/candidates/<candidate-id>`
//! inside the source repository so the candidate stays resolvable after the
//! job-private Worktree is removed. The candidate id is the frozen candidate
//! commit id itself: the freeze is deterministic, so a repeated freeze maps to
//! the same ref name and only confirms the already-recorded value.
//!
//! Failure is fail closed: every ref write either succeeds and is confirmed by
//! an exact re-read, or returns an error that the freeze path must surface so
//! the Candidate is never announced as deliverable without its stable ref.
//! The returned receipt is the recorded summary of one ref write.

use std::fmt;
use std::path::Path;
use std::process::Command;

/// Ref namespace prefix for every frozen Worker candidate.
pub const CANDIDATE_REF_PREFIX: &str = "refs/winwincode/candidates/";

/// Ref update summary recorded with every candidate ref write.
pub const CANDIDATE_REF_UPDATE_MESSAGE: &str = "winwincode: freeze candidate";

/// Stable candidate-ref failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateRefErrorCode {
    InvalidInput,
    Conflict,
    Git,
}

/// Bounded candidate-ref failure which does not retain repository content.
#[derive(Debug)]
pub struct CandidateRefError {
    code: CandidateRefErrorCode,
    message: String,
}

impl CandidateRefError {
    fn new(code: CandidateRefErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(CandidateRefErrorCode::InvalidInput, message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(CandidateRefErrorCode::Conflict, message)
    }

    fn git(context: &str, stderr: &str) -> Self {
        Self::new(
            CandidateRefErrorCode::Git,
            format!("{context}: {}", stderr.trim()),
        )
    }

    /// Returns the stable machine-readable failure category.
    #[must_use]
    pub const fn code(&self) -> CandidateRefErrorCode {
        self.code
    }
}

impl fmt::Display for CandidateRefError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CandidateRefError {}

/// Recorded summary of one stable candidate ref write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRefReceipt {
    /// Full ref name inside the source repository.
    pub ref_name: String,
    /// Candidate identity; exactly the frozen candidate commit id.
    pub candidate_id: String,
    /// Frozen candidate commit the ref resolves to.
    pub candidate_commit_id: String,
    /// Whether the ref already resolved to the same commit (idempotent replay).
    pub preexisting: bool,
}

/// Returns the exact stable ref name for one candidate id.
///
/// # Errors
///
/// Rejects an id that is not a lowercase full Git object id, which keeps the
/// ref name inside the `refs/winwincode/candidates` namespace.
pub fn candidate_ref_name(candidate_id: &str) -> Result<String, CandidateRefError> {
    validate_candidate_id(candidate_id)?;
    Ok(format!("{CANDIDATE_REF_PREFIX}{candidate_id}"))
}

/// Records the stable candidate ref for one frozen candidate commit.
///
/// The ref is written in the source repository shared with the detached
/// Worktree, so it keeps resolving after the Worktree is removed. Re-recording
/// an already-recorded candidate is an idempotent success.
///
/// # Errors
///
/// Rejects an invalid or unknown candidate commit, a conflicting existing ref
/// value, and any Git failure; callers must treat every error as "the
/// candidate is not deliverable".
pub fn create_candidate_ref(
    repository: &Path,
    candidate_commit_id: &str,
) -> Result<CandidateRefReceipt, CandidateRefError> {
    validate_candidate_id(candidate_commit_id)?;
    let candidate_commit_id = resolve_candidate_commit(repository, candidate_commit_id)?;
    let ref_name = candidate_ref_name(&candidate_commit_id)?;
    if let Some(existing) = read_ref(repository, &ref_name)? {
        if existing == candidate_commit_id {
            return Ok(CandidateRefReceipt {
                ref_name,
                candidate_id: candidate_commit_id.clone(),
                candidate_commit_id,
                preexisting: true,
            });
        }
        return Err(CandidateRefError::conflict(
            "candidate ref already resolves to a different commit",
        ));
    }
    let mut command = git_command(repository);
    command.args([
        "update-ref",
        "-m",
        CANDIDATE_REF_UPDATE_MESSAGE,
        "--",
        ref_name.as_str(),
        candidate_commit_id.as_str(),
    ]);
    command_status(&mut command, "Git candidate ref cannot be created")?;
    let confirmed = read_ref(repository, &ref_name)?;
    if confirmed.as_deref() != Some(candidate_commit_id.as_str()) {
        return Err(CandidateRefError::new(
            CandidateRefErrorCode::Git,
            "Git candidate ref does not resolve to the frozen commit",
        ));
    }
    Ok(CandidateRefReceipt {
        ref_name,
        candidate_id: candidate_commit_id.clone(),
        candidate_commit_id,
        preexisting: false,
    })
}

fn validate_candidate_id(candidate_id: &str) -> Result<(), CandidateRefError> {
    let valid = matches!(candidate_id.len(), 40 | 64)
        && candidate_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if valid {
        return Ok(());
    }
    Err(CandidateRefError::invalid(
        "candidate id must be a full lowercase Git object id",
    ))
}

fn resolve_candidate_commit(
    repository: &Path,
    candidate_commit_id: &str,
) -> Result<String, CandidateRefError> {
    let revision = format!("{candidate_commit_id}^{{commit}}");
    let resolved = git_text(
        repository,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            revision.as_str(),
        ],
        "Git candidate commit cannot be resolved",
    )?;
    if resolved != candidate_commit_id {
        return Err(CandidateRefError::conflict(
            "candidate id does not resolve to its exact commit",
        ));
    }
    Ok(resolved)
}

fn read_ref(repository: &Path, ref_name: &str) -> Result<Option<String>, CandidateRefError> {
    let mut command = git_command(repository);
    command.args([
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        ref_name,
    ]);
    let output = command.output().map_err(|error| {
        CandidateRefError::new(
            CandidateRefErrorCode::Git,
            format!("Git candidate ref cannot be read: {error}"),
        )
    })?;
    if output.status.success() {
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|_| CandidateRefError::invalid("Git candidate ref output is not UTF-8"))?;
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() || text.contains(['\r', '\n']) {
            return Err(CandidateRefError::invalid(
                "Git candidate ref output is not a single identity",
            ));
        }
        return Ok(Some(text.to_owned()));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(CandidateRefError::git(
        "Git candidate ref cannot be read",
        &stderr,
    ))
}

fn git_text(
    repository: &Path,
    arguments: &[&str],
    context: &str,
) -> Result<String, CandidateRefError> {
    let mut command = git_command(repository);
    command.args(arguments);
    let output = command.output().map_err(|error| {
        CandidateRefError::new(CandidateRefErrorCode::Git, format!("{context}: {error}"))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CandidateRefError::git(context, &stderr));
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| CandidateRefError::invalid("Git output is not UTF-8"))?;
    let text = text.trim_end_matches(['\r', '\n']);
    if text.is_empty() || text.contains(['\r', '\n']) {
        return Err(CandidateRefError::invalid(
            "Git returned an invalid single-line identity",
        ));
    }
    Ok(text.to_owned())
}

fn command_status(command: &mut Command, context: &str) -> Result<(), CandidateRefError> {
    let output = command.output().map_err(|error| {
        CandidateRefError::new(CandidateRefErrorCode::Git, format!("{context}: {error}"))
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(CandidateRefError::git(context, &stderr))
}

fn git_command(repository: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository);
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}
