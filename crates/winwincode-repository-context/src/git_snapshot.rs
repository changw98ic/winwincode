use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{RepositoryContextError, RepositoryFile};

#[derive(Clone, Debug)]
pub struct GitRepositorySnapshot {
    root: PathBuf,
    baseline_sha: String,
    files: Vec<RepositoryFile>,
}

impl GitRepositorySnapshot {
    /// Opens the immutable tree for an exact Git commit SHA.
    ///
    /// # Errors
    ///
    /// Returns an error when the SHA is symbolic, missing, not a commit, or
    /// when Git cannot list the baseline tree.
    pub fn open(root: &Path, baseline_sha: &str) -> Result<Self, RepositoryContextError> {
        validate_exact_sha(baseline_sha)?;
        let resolved = git_output(
            root,
            "resolve baseline",
            [
                "rev-parse",
                "--verify",
                &format!("{baseline_sha}^{{commit}}"),
            ],
        )
        .map_err(|error| match error {
            RepositoryContextError::GitCommand { .. } => {
                RepositoryContextError::BaselineNotFound(baseline_sha.to_owned())
            }
            other => other,
        })?;
        let resolved = String::from_utf8_lossy(&resolved)
            .trim()
            .to_ascii_lowercase();
        if resolved != baseline_sha.to_ascii_lowercase() {
            return Err(RepositoryContextError::InvalidBaselineSha(
                baseline_sha.to_owned(),
            ));
        }

        let output = git_output_bytes(
            root,
            "list baseline files",
            ["ls-tree", "-rlz", baseline_sha],
        )?;
        let mut files = output
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
            .map(parse_ls_tree_record)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.path.cmp(&right.path));

        Ok(Self {
            root: root.to_path_buf(),
            baseline_sha: resolved,
            files,
        })
    }

    #[must_use]
    pub fn baseline_sha(&self) -> &str {
        &self.baseline_sha
    }

    #[must_use]
    pub fn files(&self) -> &[RepositoryFile] {
        &self.files
    }

    /// Reads one UTF-8 file from the immutable baseline tree.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is absent, is not a blob, or does not
    /// contain UTF-8 text.
    pub fn read_text(&self, path: &str) -> Result<String, RepositoryContextError> {
        let object = format!("{}:{path}", self.baseline_sha);
        let output = git_output_bytes(&self.root, "read baseline file", ["show", &object])
            .map_err(|error| RepositoryContextError::SnapshotRead {
                path: path.to_owned(),
                detail: error.to_string(),
            })?;
        String::from_utf8(output).map_err(|error| RepositoryContextError::SnapshotRead {
            path: path.to_owned(),
            detail: error.to_string(),
        })
    }
}

fn validate_exact_sha(sha: &str) -> Result<(), RepositoryContextError> {
    if matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(RepositoryContextError::InvalidBaselineSha(sha.to_owned()))
    }
}

fn parse_ls_tree_record(record: &[u8]) -> Result<Option<RepositoryFile>, RepositoryContextError> {
    let record = String::from_utf8_lossy(record);
    let (metadata, path) =
        record
            .split_once('\t')
            .ok_or_else(|| RepositoryContextError::GitCommand {
                operation: "list baseline files",
                detail: format!("unexpected ls-tree record: {record}"),
            })?;
    let mut fields = metadata.split_whitespace();
    let _mode = fields.next();
    let object_type = fields.next();
    let object_id = fields.next();
    let size = fields.next();
    if object_type != Some("blob") {
        return Ok(None);
    }
    let object_id = object_id.ok_or_else(|| RepositoryContextError::GitCommand {
        operation: "list baseline files",
        detail: format!("missing object id in record: {record}"),
    })?;
    let bytes = size.and_then(|value| value.parse::<u64>().ok());
    Ok(Some(RepositoryFile {
        path: path.to_owned(),
        bytes,
        content_fingerprint: object_id.to_owned(),
    }))
}

fn git_output<const N: usize>(
    root: &Path,
    operation: &'static str,
    arguments: [&str; N],
) -> Result<Vec<u8>, RepositoryContextError> {
    git_output_bytes(root, operation, arguments)
}

fn git_output_bytes<I, S>(
    root: &Path,
    operation: &'static str,
    arguments: I,
) -> Result<Vec<u8>, RepositoryContextError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .map_err(|error| RepositoryContextError::GitCommand {
            operation,
            detail: error.to_string(),
        })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(RepositoryContextError::GitCommand {
            operation,
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}
