// SPDX-License-Identifier: Apache-2.0

//! Trusted local Git reconstruction for immutable candidate Artifacts.
//!
//! The Artifact contains only a candidate commit hint. This adapter reads the
//! controlled repository and rebuilds every commit/tree/diff/path/hunk value;
//! values reported by a Worker are never copied into a validated source fact.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ArtifactError, ArtifactErrorKind, ArtifactObject, ArtifactRecord};

const CANDIDATE_MANIFEST_SCHEMA_VERSION: u8 = 1;
const CANDIDATE_MEDIA_TYPE: &str = "application/vnd.winwincode.git-candidate+json";
const MAX_CHANGED_PATHS: usize = 100_000;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_DIFF_BYTES: usize = 268_435_456;

/// Minimal Worker-produced hint stored inside a candidate Artifact.
///
/// Tree, diff, path, and object identities are deliberately absent. The local
/// resolver obtains them from Git rather than trusting a report.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateSourceManifest {
    schema_version: u8,
    candidate_commit_id: String,
}

impl CandidateSourceManifest {
    /// Creates the one canonical candidate Artifact manifest.
    ///
    /// # Errors
    ///
    /// Rejects a value that cannot name a SHA-1 or SHA-256 Git commit object.
    pub fn new(candidate_commit_id: impl Into<String>) -> Result<Self, ArtifactError> {
        let candidate_commit_id = candidate_commit_id.into();
        git_object_id(&candidate_commit_id, "candidateCommitId")?;
        Ok(Self {
            schema_version: CANDIDATE_MANIFEST_SCHEMA_VERSION,
            candidate_commit_id,
        })
    }

    /// Encodes the strict canonical JSON stored as Artifact bytes.
    ///
    /// # Errors
    ///
    /// Returns an adapter error if canonical serialization fails.
    pub fn encode(&self) -> Result<Vec<u8>, ArtifactError> {
        serde_json::to_vec(self).map_err(|error| {
            ArtifactError::adapter(format!(
                "candidate source manifest cannot be encoded: {error}"
            ))
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CandidateSourceManifestInput {
    schema_version: u8,
    candidate_commit_id: String,
}

/// State of one path rebuilt at the candidate commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitSourcePathState {
    Present,
    Deleted,
}

/// One exact path/object relation rebuilt from Git.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitSourcePath {
    path: String,
    state: GitSourcePathState,
    object_id: Option<String>,
}

impl GitSourcePath {
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub const fn state(&self) -> GitSourcePathState {
        self.state
    }

    #[must_use]
    pub fn object_id(&self) -> Option<&str> {
        self.object_id.as_deref()
    }
}

/// One hunk identity computed from the exact Git diff bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitSourceHunk {
    file_path: String,
    hunk_sha256: String,
}

impl GitSourceHunk {
    #[must_use]
    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    #[must_use]
    pub fn hunk_sha256(&self) -> &str {
        &self.hunk_sha256
    }
}

/// Opaque result issued only after an Artifact and controlled repository agree.
///
/// It is intentionally not deserializable and has no public constructor.
///
/// ```compile_fail
/// use winwincode_storage::ValidatedGitSourceArtifact;
///
/// let _: ValidatedGitSourceArtifact = serde_json::from_str("{}")?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedGitSourceArtifact {
    repository_locator: String,
    requested_base_revision: String,
    base_commit_id: String,
    base_tree_id: String,
    candidate_commit_id: String,
    candidate_tree_id: String,
    diff_sha256: String,
    changed_paths: Vec<GitSourcePath>,
    changed_hunks: Vec<GitSourceHunk>,
    artifact: ArtifactRecord,
}

/// Replaceable trusted source adapter. The opaque result can only be created
/// inside this crate, so callers may select an adapter but cannot self-report
/// commit/tree/diff/path identities.
pub trait GitSourceResolver: Send {
    /// Rebuilds one candidate identity from controlled source and exact
    /// Artifact bytes.
    ///
    /// # Errors
    ///
    /// Fails closed when source or Artifact facts do not agree.
    fn resolve_candidate(
        &self,
        artifact: &ArtifactObject,
        repository_locator: &str,
        base_revision: &str,
    ) -> Result<ValidatedGitSourceArtifact, ArtifactError>;
}

impl ValidatedGitSourceArtifact {
    #[must_use]
    pub fn repository_locator(&self) -> &str {
        &self.repository_locator
    }

    #[must_use]
    pub fn requested_base_revision(&self) -> &str {
        &self.requested_base_revision
    }

    #[must_use]
    pub fn base_commit_id(&self) -> &str {
        &self.base_commit_id
    }

    #[must_use]
    pub fn base_tree_id(&self) -> &str {
        &self.base_tree_id
    }

    #[must_use]
    pub fn candidate_commit_id(&self) -> &str {
        &self.candidate_commit_id
    }

    #[must_use]
    pub fn candidate_tree_id(&self) -> &str {
        &self.candidate_tree_id
    }

    #[must_use]
    pub fn diff_sha256(&self) -> &str {
        &self.diff_sha256
    }

    #[must_use]
    pub fn changed_paths(&self) -> &[GitSourcePath] {
        &self.changed_paths
    }

    #[must_use]
    pub fn changed_hunks(&self) -> &[GitSourceHunk] {
        &self.changed_hunks
    }

    #[must_use]
    pub const fn artifact(&self) -> &ArtifactRecord {
        &self.artifact
    }
}

/// Resolves only repository locators below one configured root.
pub struct LocalGitSourceResolver {
    allowed_root: PathBuf,
}

impl LocalGitSourceResolver {
    /// Opens the controlled repository root.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the root is missing or cannot be
    /// canonicalized.
    pub fn open(allowed_root: impl AsRef<Path>) -> Result<Self, ArtifactError> {
        let allowed_root = fs::canonicalize(allowed_root).map_err(|error| {
            ArtifactError::adapter(format!(
                "controlled repository root cannot be opened: {error}"
            ))
        })?;
        if !allowed_root.is_dir() {
            return Err(ArtifactError::invalid(
                "controlled repository root is not a directory",
            ));
        }
        Ok(Self { allowed_root })
    }

    /// Rebuilds one candidate identity from a complete, exact candidate
    /// Artifact and a repository beneath the configured root.
    ///
    /// # Errors
    ///
    /// Rejects foreign media/kind, malformed or extra manifest fields,
    /// missing/diverged commits, empty changes, non-portable paths, Git command
    /// failures, and any unsupported diff size.
    pub fn resolve_candidate(
        &self,
        artifact: &ArtifactObject,
        repository_locator: &str,
        base_revision: &str,
    ) -> Result<ValidatedGitSourceArtifact, ArtifactError> {
        let candidate_commit = candidate_manifest_commit(artifact)?;
        bounded_locator(repository_locator)?;
        bounded_revision(base_revision)?;
        let repository = controlled_repository(&self.allowed_root, repository_locator)?;
        assert_git_repository(&repository)?;
        rebuild_candidate_source(
            &repository,
            repository_locator,
            base_revision,
            &candidate_commit,
            artifact.metadata().clone(),
        )
    }
}

fn candidate_manifest_commit(artifact: &ArtifactObject) -> Result<String, ArtifactError> {
    if artifact.metadata().kind() != "candidate"
        || artifact.metadata().media_type() != CANDIDATE_MEDIA_TYPE
    {
        return Err(ArtifactError::invalid(
            "Git source resolver requires one canonical candidate Artifact",
        ));
    }
    let manifest: CandidateSourceManifestInput =
        serde_json::from_slice(artifact.bytes()).map_err(|_| {
            ArtifactError::invalid("candidate Artifact is not the strict canonical source manifest")
        })?;
    if manifest.schema_version != CANDIDATE_MANIFEST_SCHEMA_VERSION {
        return Err(ArtifactError::invalid(
            "candidate source manifest schema version is unsupported",
        ));
    }
    git_object_id(&manifest.candidate_commit_id, "candidateCommitId")?;
    let canonical = CandidateSourceManifest::new(manifest.candidate_commit_id.clone())?.encode()?;
    if artifact.bytes() != canonical {
        return Err(ArtifactError::invalid(
            "candidate Artifact bytes are not the canonical source manifest",
        ));
    }
    Ok(manifest.candidate_commit_id)
}

fn controlled_repository(
    allowed_root: &Path,
    repository_locator: &str,
) -> Result<PathBuf, ArtifactError> {
    let repository = fs::canonicalize(allowed_root.join(repository_locator)).map_err(|error| {
        ArtifactError::new(
            ArtifactErrorKind::NotFound,
            format!("controlled Git repository cannot be opened: {error}"),
        )
    })?;
    if !repository.starts_with(allowed_root) || !repository.is_dir() {
        return Err(ArtifactError::new(
            ArtifactErrorKind::PermissionDenied,
            "repository locator escapes the configured source root",
        ));
    }
    Ok(repository)
}

fn rebuild_candidate_source(
    repository: &Path,
    repository_locator: &str,
    base_revision: &str,
    candidate_revision: &str,
    artifact: ArtifactRecord,
) -> Result<ValidatedGitSourceArtifact, ArtifactError> {
    let base_commit_id = rev_parse_commit(repository, base_revision, "base revision")?;
    let candidate_commit_id = rev_parse_commit(repository, candidate_revision, "candidate commit")?;
    if base_commit_id.len() != candidate_commit_id.len() {
        return Err(ArtifactError::invalid(
            "base and candidate commits use different Git object formats",
        ));
    }
    require_descendant(repository, &base_commit_id, &candidate_commit_id)?;
    let base_tree_id = rev_parse_tree(repository, &base_commit_id, "base tree")?;
    let candidate_tree_id = rev_parse_tree(repository, &candidate_commit_id, "candidate tree")?;
    let revision_range = format!("{base_commit_id}..{candidate_commit_id}");
    let diff = candidate_diff(repository, &revision_range)?;
    let diff_sha256 = format!("{:x}", Sha256::digest(diff));
    let changed_path_names = candidate_path_names(repository, &revision_range)?;
    let (changed_paths, changed_hunks) = rebuild_changed_files(
        repository,
        &candidate_commit_id,
        &revision_range,
        &changed_path_names,
    )?;
    Ok(ValidatedGitSourceArtifact {
        repository_locator: repository_locator.to_owned(),
        requested_base_revision: base_revision.to_owned(),
        base_commit_id,
        base_tree_id,
        candidate_commit_id,
        candidate_tree_id,
        diff_sha256,
        changed_paths,
        changed_hunks,
        artifact,
    })
}

fn require_descendant(
    repository: &Path,
    base_commit_id: &str,
    candidate_commit_id: &str,
) -> Result<(), ArtifactError> {
    let ancestor = git_status(
        repository,
        &[
            "merge-base".into(),
            "--is-ancestor".into(),
            base_commit_id.into(),
            candidate_commit_id.into(),
        ],
    )?;
    if !ancestor.status.success() {
        return Err(ArtifactError::conflict(
            "candidate commit is not descended from the pinned base commit",
        ));
    }
    Ok(())
}

fn candidate_diff(repository: &Path, revision_range: &str) -> Result<Vec<u8>, ArtifactError> {
    let diff = git_output(
        repository,
        &[
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--binary".into(),
            "--full-index".into(),
            revision_range.into(),
        ],
        "candidate diff",
    )?;
    if diff.len() > MAX_DIFF_BYTES {
        return Err(ArtifactError::invalid(
            "candidate diff exceeds the supported in-memory verification limit",
        ));
    }
    Ok(diff)
}

fn candidate_path_names(
    repository: &Path,
    revision_range: &str,
) -> Result<Vec<String>, ArtifactError> {
    let paths = git_output(
        repository,
        &[
            "diff".into(),
            "--name-only".into(),
            "-z".into(),
            revision_range.into(),
        ],
        "candidate changed paths",
    )?;
    let paths = decode_paths(&paths)?;
    if paths.is_empty() {
        return Err(ArtifactError::conflict(
            "candidate commit changes no repository path",
        ));
    }
    if paths.len() > MAX_CHANGED_PATHS {
        return Err(ArtifactError::invalid(
            "candidate changed paths exceed the supported limit",
        ));
    }
    Ok(paths)
}

fn rebuild_changed_files(
    repository: &Path,
    candidate_commit_id: &str,
    revision_range: &str,
    paths: &[String],
) -> Result<(Vec<GitSourcePath>, Vec<GitSourceHunk>), ArtifactError> {
    let mut rebuilt_paths = Vec::with_capacity(paths.len());
    let mut changed_hunks = Vec::new();
    for path in paths {
        let (state, object_id) = candidate_path_object(repository, candidate_commit_id, path)?;
        rebuilt_paths.push(GitSourcePath {
            path: path.clone(),
            state,
            object_id,
        });
        let path_diff = candidate_path_diff(repository, revision_range, path)?;
        for hunk_sha256 in hunk_digests(&path_diff)? {
            changed_hunks.push(GitSourceHunk {
                file_path: path.clone(),
                hunk_sha256,
            });
        }
    }
    rebuilt_paths.sort_by(|left, right| left.path.cmp(&right.path));
    changed_hunks.sort_by(|left, right| {
        left.file_path
            .cmp(&right.file_path)
            .then_with(|| left.hunk_sha256.cmp(&right.hunk_sha256))
    });
    Ok((rebuilt_paths, changed_hunks))
}

fn candidate_path_object(
    repository: &Path,
    candidate_commit_id: &str,
    path: &str,
) -> Result<(GitSourcePathState, Option<String>), ArtifactError> {
    let object = git_status(
        repository,
        &[
            "rev-parse".into(),
            "--verify".into(),
            format!("{candidate_commit_id}:{path}").into(),
        ],
    )?;
    if object.status.success() {
        let value = utf8_line(object.stdout, "candidate path object")?;
        git_object_id(&value, "candidate path object")?;
        if value.len() != candidate_commit_id.len() {
            return Err(ArtifactError::corrupt(
                "candidate path object uses a different Git object format",
            ));
        }
        return Ok((GitSourcePathState::Present, Some(value)));
    }
    if object.status.code() == Some(128) {
        return Ok((GitSourcePathState::Deleted, None));
    }
    Err(git_failure("candidate path object", &object))
}

fn candidate_path_diff(
    repository: &Path,
    revision_range: &str,
    path: &str,
) -> Result<Vec<u8>, ArtifactError> {
    git_output(
        repository,
        &[
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--binary".into(),
            "--full-index".into(),
            revision_range.into(),
            "--".into(),
            path.into(),
        ],
        "candidate path diff",
    )
}

impl GitSourceResolver for LocalGitSourceResolver {
    fn resolve_candidate(
        &self,
        artifact: &ArtifactObject,
        repository_locator: &str,
        base_revision: &str,
    ) -> Result<ValidatedGitSourceArtifact, ArtifactError> {
        Self::resolve_candidate(self, artifact, repository_locator, base_revision)
    }
}

fn assert_git_repository(repository: &Path) -> Result<(), ArtifactError> {
    let value = git_output(
        repository,
        &["rev-parse".into(), "--is-inside-work-tree".into()],
        "repository identity",
    )?;
    if value != b"true\n" {
        return Err(ArtifactError::invalid(
            "configured source is not one Git worktree",
        ));
    }
    Ok(())
}

fn rev_parse_commit(
    repository: &Path,
    revision: &str,
    field: &str,
) -> Result<String, ArtifactError> {
    let value = git_output(
        repository,
        &[
            "rev-parse".into(),
            "--verify".into(),
            format!("{revision}^{{commit}}").into(),
        ],
        field,
    )?;
    let value = utf8_line(value, field)?;
    git_object_id(&value, field)?;
    Ok(value)
}

fn rev_parse_tree(repository: &Path, commit: &str, field: &str) -> Result<String, ArtifactError> {
    let value = git_output(
        repository,
        &["rev-parse".into(), format!("{commit}^{{tree}}").into()],
        field,
    )?;
    let value = utf8_line(value, field)?;
    git_object_id(&value, field)?;
    if value.len() != commit.len() {
        return Err(ArtifactError::corrupt(
            "Git commit and tree use different object formats",
        ));
    }
    Ok(value)
}

fn decode_paths(bytes: &[u8]) -> Result<Vec<String>, ArtifactError> {
    let mut paths = Vec::new();
    for value in bytes
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
    {
        let path = std::str::from_utf8(value)
            .map_err(|_| ArtifactError::invalid("candidate path is not UTF-8"))?
            .to_owned();
        portable_path(&path)?;
        paths.push(path);
    }
    paths.sort();
    if paths.windows(2).any(|values| values[0] == values[1]) {
        return Err(ArtifactError::corrupt(
            "candidate changed path list contains duplicates",
        ));
    }
    Ok(paths)
}

fn hunk_digests(diff: &[u8]) -> Result<Vec<String>, ArtifactError> {
    if diff.is_empty() {
        return Err(ArtifactError::corrupt(
            "candidate changed path has no exact Git diff bytes",
        ));
    }
    let mut line_starts = vec![0_usize];
    for (index, byte) in diff.iter().enumerate() {
        if *byte == b'\n' && index + 1 < diff.len() {
            line_starts.push(index + 1);
        }
    }
    let hunk_starts = line_starts
        .into_iter()
        .filter(|start| diff[*start..].starts_with(b"@@ "))
        .collect::<Vec<_>>();
    if hunk_starts.is_empty() {
        return Ok(vec![format!("{:x}", Sha256::digest(diff))]);
    }
    Ok(hunk_starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = hunk_starts.get(index + 1).copied().unwrap_or(diff.len());
            format!("{:x}", Sha256::digest(&diff[*start..end]))
        })
        .collect())
}

fn git_output(
    repository: &Path,
    arguments: &[OsString],
    operation: &str,
) -> Result<Vec<u8>, ArtifactError> {
    let output = git_status(repository, arguments)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(git_failure(operation, &output))
    }
}

fn git_status(repository: &Path, arguments: &[OsString]) -> Result<Output, ArtifactError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|error| ArtifactError::adapter(format!("Git source command failed: {error}")))
}

fn git_failure(operation: &str, output: &Output) -> ArtifactError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    ArtifactError::new(
        ArtifactErrorKind::NotFound,
        if stderr.is_empty() {
            format!("Git {operation} failed")
        } else {
            format!("Git {operation} failed: {stderr}")
        },
    )
}

fn utf8_line(bytes: Vec<u8>, field: &str) -> Result<String, ArtifactError> {
    let value = String::from_utf8(bytes)
        .map_err(|_| ArtifactError::corrupt(format!("Git {field} is not UTF-8")))?;
    let value = value.strip_suffix('\n').unwrap_or(&value);
    if value.is_empty() || value.contains(['\r', '\n']) {
        return Err(ArtifactError::corrupt(format!(
            "Git {field} is not one exact value"
        )));
    }
    Ok(value.to_owned())
}

fn bounded_locator(value: &str) -> Result<(), ArtifactError> {
    if value.is_empty()
        || value.len() > MAX_PATH_BYTES
        || Path::new(value).is_absolute()
        || value.contains('\\')
        || value.bytes().any(|byte| byte <= 31 || byte == 127)
        || value
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(ArtifactError::invalid(
            "repository locator is not a portable controlled relative path",
        ));
    }
    Ok(())
}

fn bounded_revision(value: &str) -> Result<(), ArtifactError> {
    if value.is_empty()
        || value.len() > 512
        || value.starts_with('-')
        || value.bytes().any(|byte| byte <= 32 || byte == 127)
        || value.contains(['\0', '\\'])
    {
        return Err(ArtifactError::invalid("base revision is invalid"));
    }
    Ok(())
}

fn portable_path(value: &str) -> Result<(), ArtifactError> {
    if value.is_empty()
        || value.len() > MAX_PATH_BYTES
        || value.starts_with('/')
        || value.contains('\\')
        || value.bytes().any(|byte| byte <= 31 || byte == 127)
        || value
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(ArtifactError::invalid(
            "candidate path is not a portable repository-relative path",
        ));
    }
    Ok(())
}

fn git_object_id(value: &str, field: &str) -> Result<(), ArtifactError> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(ArtifactError::invalid(format!(
            "{field} is not a lowercase Git object identity"
        )));
    }
    Ok(())
}
