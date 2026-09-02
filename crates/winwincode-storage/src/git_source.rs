// SPDX-License-Identifier: Apache-2.0

//! Trusted local Git reconstruction for immutable candidate Artifacts.
//!
//! The Artifact contains only a candidate commit hint. This adapter reads the
//! controlled repository and rebuilds every commit/tree/diff/path/hunk value;
//! values reported by a Worker are never copied into a validated source fact.

use std::collections::BTreeMap;
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

/// Closed file status rebuilt from one exact retained Candidate range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitCandidateReviewFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

/// Closed content classification for exact Git diff bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitCandidateReviewFileEncoding {
    Utf8,
    Binary,
    Unknown8Bit,
}

/// Secret-free file metadata rebuilt from controlled Git.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCandidateReviewFile {
    path: String,
    old_path: Option<String>,
    status: GitCandidateReviewFileStatus,
    additions: Option<u64>,
    deletions: Option<u64>,
    binary: bool,
    encoding: GitCandidateReviewFileEncoding,
}

impl GitCandidateReviewFile {
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub fn old_path(&self) -> Option<&str> {
        self.old_path.as_deref()
    }

    #[must_use]
    pub const fn status(&self) -> GitCandidateReviewFileStatus {
        self.status
    }

    #[must_use]
    pub const fn additions(&self) -> Option<u64> {
        self.additions
    }

    #[must_use]
    pub const fn deletions(&self) -> Option<u64> {
        self.deletions
    }

    #[must_use]
    pub const fn is_binary(&self) -> bool {
        self.binary
    }

    #[must_use]
    pub const fn encoding(&self) -> GitCandidateReviewFileEncoding {
        self.encoding
    }
}

/// Opaque, validated changed-file inventory for one exact Candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedGitCandidateReview {
    candidate_commit_id: String,
    candidate_tree_id: String,
    diff_sha256: String,
    files: Vec<GitCandidateReviewFile>,
}

impl ValidatedGitCandidateReview {
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
    pub fn files(&self) -> &[GitCandidateReviewFile] {
        &self.files
    }
}

/// Opaque, validated unified-diff bytes for one trusted Candidate path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedGitCandidateDiff {
    path: String,
    old_path: Option<String>,
    status: GitCandidateReviewFileStatus,
    file_diff_sha256: String,
    binary: bool,
    encoding: GitCandidateReviewFileEncoding,
    bytes: Vec<u8>,
}

impl ValidatedGitCandidateDiff {
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub fn old_path(&self) -> Option<&str> {
        self.old_path.as_deref()
    }

    #[must_use]
    pub const fn status(&self) -> GitCandidateReviewFileStatus {
        self.status
    }

    #[must_use]
    pub fn file_diff_sha256(&self) -> &str {
        &self.file_diff_sha256
    }

    #[must_use]
    pub const fn is_binary(&self) -> bool {
        self.binary
    }

    #[must_use]
    pub const fn encoding(&self) -> GitCandidateReviewFileEncoding {
        self.encoding
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
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

    /// Returns the canonical root used to resolve local repositories, when
    /// this adapter has one.  The Control Plane uses this exact root for Git
    /// retention so source reconstruction and durable references cannot drift
    /// to different repository trees.  Remote adapters may leave this unset.
    fn controlled_repository_root(&self) -> Option<&Path> {
        None
    }

    /// Rebuilds a closed changed-file inventory from an already validated
    /// Candidate source.
    ///
    /// Adapter implementations that cannot provide review reads fail closed.
    ///
    /// # Errors
    ///
    /// Returns an Artifact error when the exact retained source cannot be
    /// revalidated or this adapter does not implement review reads.
    fn candidate_review(
        &self,
        _source: &ValidatedGitSourceArtifact,
    ) -> Result<ValidatedGitCandidateReview, ArtifactError> {
        Err(ArtifactError::adapter(
            "Git Candidate review reads are unavailable",
        ))
    }

    /// Rebuilds exact unified-diff bytes for one path selected from the trusted
    /// Candidate inventory.
    ///
    /// Adapter implementations that cannot provide review reads fail closed.
    ///
    /// # Errors
    ///
    /// Returns an Artifact error when the source or path is invalid, the
    /// retained Git facts changed, or this adapter does not implement review
    /// reads.
    fn candidate_diff(
        &self,
        _source: &ValidatedGitSourceArtifact,
        _path: &str,
    ) -> Result<ValidatedGitCandidateDiff, ArtifactError> {
        Err(ArtifactError::adapter(
            "Git Candidate diff reads are unavailable",
        ))
    }
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

    /// Returns the canonical repository root selected at adapter startup.
    #[must_use]
    pub fn controlled_repository_root(&self) -> &Path {
        &self.allowed_root
    }

    fn candidate_review(
        &self,
        source: &ValidatedGitSourceArtifact,
    ) -> Result<ValidatedGitCandidateReview, ArtifactError> {
        let repository = self.revalidate_candidate_source(source)?;
        let revision_range = format!("{}..{}", source.base_commit_id, source.candidate_commit_id);
        let statuses = candidate_file_statuses(&repository, &revision_range)?;
        let stats = candidate_file_stats(&repository, &revision_range)?;
        if statuses.len() != stats.len()
            || statuses
                .iter()
                .any(|status| !stats.contains_key(status.path.as_str()))
        {
            return Err(ArtifactError::corrupt(
                "Candidate changed-file status and stat inventories differ",
            ));
        }
        let mut files = Vec::with_capacity(statuses.len());
        for status in statuses {
            let stat = stats
                .get(status.path.as_str())
                .ok_or_else(|| ArtifactError::corrupt("Candidate changed-file stat is missing"))?;
            let path_diff = candidate_path_diff(&repository, &revision_range, &status.path)?;
            if path_diff.is_empty() || path_diff.len() > MAX_DIFF_BYTES {
                return Err(ArtifactError::corrupt(
                    "Candidate path diff is empty or over limit",
                ));
            }
            files.push(GitCandidateReviewFile {
                path: status.path,
                old_path: status.old_path,
                status: status.status,
                additions: stat.additions,
                deletions: stat.deletions,
                binary: stat.binary,
                encoding: candidate_file_encoding(stat.binary, &path_diff),
            });
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        if files.is_empty() || files.len() > MAX_CHANGED_PATHS {
            return Err(ArtifactError::corrupt(
                "Candidate changed-file inventory is empty or over limit",
            ));
        }
        Ok(ValidatedGitCandidateReview {
            candidate_commit_id: source.candidate_commit_id.clone(),
            candidate_tree_id: source.candidate_tree_id.clone(),
            diff_sha256: source.diff_sha256.clone(),
            files,
        })
    }

    fn candidate_diff(
        &self,
        source: &ValidatedGitSourceArtifact,
        path: &str,
    ) -> Result<ValidatedGitCandidateDiff, ArtifactError> {
        portable_path(path)?;
        let review = self.candidate_review(source)?;
        let file = review
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or_else(|| {
                ArtifactError::new(ArtifactErrorKind::NotFound, "Candidate path is not changed")
            })?;
        let repository = controlled_repository(&self.allowed_root, &source.repository_locator)?;
        let revision_range = format!("{}..{}", source.base_commit_id, source.candidate_commit_id);
        let bytes = candidate_path_diff(&repository, &revision_range, path)?;
        if bytes.is_empty() || bytes.len() > MAX_DIFF_BYTES {
            return Err(ArtifactError::corrupt(
                "Candidate path diff is empty or over limit",
            ));
        }
        Ok(ValidatedGitCandidateDiff {
            path: path.to_owned(),
            old_path: file.old_path.clone(),
            status: file.status,
            file_diff_sha256: format!("{:x}", Sha256::digest(&bytes)),
            binary: file.binary,
            encoding: file.encoding,
            bytes,
        })
    }

    fn revalidate_candidate_source(
        &self,
        source: &ValidatedGitSourceArtifact,
    ) -> Result<PathBuf, ArtifactError> {
        let repository = controlled_repository(&self.allowed_root, &source.repository_locator)?;
        assert_git_repository(&repository)?;
        let base_tree = rev_parse_tree(&repository, &source.base_commit_id, "base tree")?;
        let candidate_tree =
            rev_parse_tree(&repository, &source.candidate_commit_id, "candidate tree")?;
        if base_tree != source.base_tree_id || candidate_tree != source.candidate_tree_id {
            return Err(ArtifactError::conflict(
                "Candidate commit/tree identity changed before review read",
            ));
        }
        let revision_range = format!("{}..{}", source.base_commit_id, source.candidate_commit_id);
        let diff = candidate_diff(&repository, &revision_range)?;
        if format!("{:x}", Sha256::digest(&diff)) != source.diff_sha256 {
            return Err(ArtifactError::conflict(
                "Candidate diff digest changed before review read",
            ));
        }
        Ok(repository)
    }
}

#[derive(Debug)]
struct CandidateFileStatusFact {
    path: String,
    old_path: Option<String>,
    status: GitCandidateReviewFileStatus,
}

#[derive(Debug)]
struct CandidateFileStatFact {
    additions: Option<u64>,
    deletions: Option<u64>,
    binary: bool,
}

fn candidate_file_statuses(
    repository: &Path,
    revision_range: &str,
) -> Result<Vec<CandidateFileStatusFact>, ArtifactError> {
    let bytes = git_output(
        repository,
        &[
            "diff".into(),
            "--name-status".into(),
            "-z".into(),
            "--find-renames=50%".into(),
            "--find-copies=50%".into(),
            revision_range.into(),
        ],
        "Candidate changed-file statuses",
    )?;
    let fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let mut index = 0_usize;
    let mut statuses = Vec::new();
    while index < fields.len() {
        let token = std::str::from_utf8(fields[index])
            .map_err(|_| ArtifactError::corrupt("Candidate file status is not UTF-8"))?;
        index += 1;
        let kind = token
            .as_bytes()
            .first()
            .copied()
            .ok_or_else(|| ArtifactError::corrupt("Candidate file status is empty"))?;
        let status = match kind {
            b'A' => GitCandidateReviewFileStatus::Added,
            b'M' => GitCandidateReviewFileStatus::Modified,
            b'D' => GitCandidateReviewFileStatus::Deleted,
            b'R' => GitCandidateReviewFileStatus::Renamed,
            b'C' => GitCandidateReviewFileStatus::Copied,
            b'T' => GitCandidateReviewFileStatus::TypeChanged,
            _ => {
                return Err(ArtifactError::corrupt(
                    "Candidate file status is unsupported",
                ));
            }
        };
        let (old_path, path) = if matches!(kind, b'R' | b'C') {
            let old = fields.get(index).ok_or_else(|| {
                ArtifactError::corrupt("Candidate renamed path is missing its old name")
            })?;
            let new = fields.get(index + 1).ok_or_else(|| {
                ArtifactError::corrupt("Candidate renamed path is missing its new name")
            })?;
            index += 2;
            (Some(decode_path(old)?), decode_path(new)?)
        } else {
            let path = fields.get(index).ok_or_else(|| {
                ArtifactError::corrupt("Candidate file status is missing its path")
            })?;
            index += 1;
            (None, decode_path(path)?)
        };
        statuses.push(CandidateFileStatusFact {
            path,
            old_path,
            status,
        });
    }
    statuses.sort_by(|left, right| left.path.cmp(&right.path));
    if statuses
        .windows(2)
        .any(|entries| entries[0].path == entries[1].path)
    {
        return Err(ArtifactError::corrupt(
            "Candidate file status inventory contains duplicate paths",
        ));
    }
    Ok(statuses)
}

fn candidate_file_stats(
    repository: &Path,
    revision_range: &str,
) -> Result<BTreeMap<String, CandidateFileStatFact>, ArtifactError> {
    let bytes = git_output(
        repository,
        &[
            "diff".into(),
            "--numstat".into(),
            "-z".into(),
            "--find-renames=50%".into(),
            "--find-copies=50%".into(),
            revision_range.into(),
        ],
        "Candidate changed-file stats",
    )?;
    let fields = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut index = 0_usize;
    let mut stats = BTreeMap::new();
    while index < fields.len() {
        let field = fields[index];
        index += 1;
        if field.is_empty() {
            continue;
        }
        let mut columns = field.splitn(3, |byte| *byte == b'\t');
        let additions = columns
            .next()
            .ok_or_else(|| ArtifactError::corrupt("Candidate numstat additions are missing"))?;
        let deletions = columns
            .next()
            .ok_or_else(|| ArtifactError::corrupt("Candidate numstat deletions are missing"))?;
        let encoded_path = columns
            .next()
            .ok_or_else(|| ArtifactError::corrupt("Candidate numstat path is missing"))?;
        let path = if encoded_path.is_empty() {
            let _old_path = fields
                .get(index)
                .ok_or_else(|| ArtifactError::corrupt("Candidate numstat old path is missing"))?;
            let new_path = fields
                .get(index + 1)
                .ok_or_else(|| ArtifactError::corrupt("Candidate numstat new path is missing"))?;
            index += 2;
            decode_path(new_path)?
        } else {
            decode_path(encoded_path)?
        };
        let binary = additions == b"-" && deletions == b"-";
        if !binary && (additions == b"-" || deletions == b"-") {
            return Err(ArtifactError::corrupt(
                "Candidate binary numstat markers differ",
            ));
        }
        let (additions, deletions) = if binary {
            (None, None)
        } else {
            (
                Some(decimal_count(additions)?),
                Some(decimal_count(deletions)?),
            )
        };
        if stats
            .insert(
                path,
                CandidateFileStatFact {
                    additions,
                    deletions,
                    binary,
                },
            )
            .is_some()
        {
            return Err(ArtifactError::corrupt(
                "Candidate numstat inventory contains duplicate paths",
            ));
        }
    }
    Ok(stats)
}

fn decode_path(bytes: &[u8]) -> Result<String, ArtifactError> {
    let path = std::str::from_utf8(bytes)
        .map_err(|_| ArtifactError::invalid("Candidate path is not UTF-8"))?
        .to_owned();
    portable_path(&path)?;
    Ok(path)
}

fn decimal_count(bytes: &[u8]) -> Result<u64, ArtifactError> {
    let value = std::str::from_utf8(bytes)
        .map_err(|_| ArtifactError::corrupt("Candidate numstat count is not UTF-8"))?
        .parse::<u64>()
        .map_err(|_| ArtifactError::corrupt("Candidate numstat count is invalid"))?;
    if value > 9_007_199_254_740_991 {
        return Err(ArtifactError::corrupt(
            "Candidate numstat count exceeds the safe integer limit",
        ));
    }
    Ok(value)
}

fn candidate_file_encoding(binary: bool, diff: &[u8]) -> GitCandidateReviewFileEncoding {
    if binary {
        GitCandidateReviewFileEncoding::Binary
    } else if std::str::from_utf8(diff).is_ok() {
        GitCandidateReviewFileEncoding::Utf8
    } else {
        GitCandidateReviewFileEncoding::Unknown8Bit
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

    fn controlled_repository_root(&self) -> Option<&Path> {
        Some(self.controlled_repository_root())
    }

    fn candidate_review(
        &self,
        source: &ValidatedGitSourceArtifact,
    ) -> Result<ValidatedGitCandidateReview, ArtifactError> {
        Self::candidate_review(self, source)
    }

    fn candidate_diff(
        &self,
        source: &ValidatedGitSourceArtifact,
        path: &str,
    ) -> Result<ValidatedGitCandidateDiff, ArtifactError> {
        Self::candidate_diff(self, source, path)
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
