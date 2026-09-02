use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use winwincode_repository_context::{
    RepositoryContextPort, RepositoryContextQuery, RepositoryContextScanner,
};

use crate::doctor::build_diagnostic_report;
use crate::git::{create_snapshot, initialize_git, inspect_repository};
use crate::{
    AttachRequest, Attachment, AttachmentOutcome, BaselineChoice, BaselineSource, DiagnosticReport,
    DoctorRequest, InitRequest, LauncherError, RepositoryInspection, SetupOutcome,
};

pub trait LocalLauncherPort: Send + Sync {
    /// Initializes Git when explicitly confirmed and binds one exact baseline.
    ///
    /// # Errors
    ///
    /// Returns a categorized error when repository inspection, Git, context
    /// detection, or local state persistence fails.
    fn initialize_repository(&self, request: &InitRequest) -> Result<SetupOutcome, LauncherError>;

    /// Binds an existing Git repository to one exact baseline.
    ///
    /// # Errors
    ///
    /// Returns a categorized error when the path is not a Git repository or
    /// when the selected baseline cannot be created, inspected, or persisted.
    fn attach_repository(&self, request: &AttachRequest) -> Result<SetupOutcome, LauncherError>;

    /// Runs product, repository, and environment diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error only when the diagnostic operation itself cannot
    /// produce a report. Individual failed checks are returned in the report.
    fn doctor(&self, request: &DoctorRequest) -> Result<DiagnosticReport, LauncherError>;
}

#[derive(Clone, Debug)]
pub struct SystemLocalLauncher {
    state_root: PathBuf,
    provider_variables: BTreeSet<String>,
}

impl SystemLocalLauncher {
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            state_root: state_root.into(),
            provider_variables: provider_variable_names(),
        }
    }

    #[must_use]
    pub fn with_provider_variables(mut self, variables: impl IntoIterator<Item = String>) -> Self {
        self.provider_variables = variables.into_iter().collect();
        self
    }

    pub(crate) fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub(crate) fn provider_variables(&self) -> &BTreeSet<String> {
        &self.provider_variables
    }

    pub(crate) fn read_attachment(&self, root: &Path) -> Result<Option<Attachment>, LauncherError> {
        let path = self.attachment_path(root);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(LauncherError::product(
                    "product.attachment-read-failed",
                    format!("无法读取仓库绑定 {}：{error}", path.display()),
                ));
            }
        };
        serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            LauncherError::product(
                "product.attachment-invalid",
                format!("仓库绑定 {} 内容无效：{error}", path.display()),
            )
        })
    }

    fn setup_existing_repository(
        &self,
        inspection: RepositoryInspection,
        choice: Option<BaselineChoice>,
        confirm_snapshot: bool,
    ) -> Result<SetupOutcome, LauncherError> {
        let root = inspection.repository_root.as_deref().ok_or_else(|| {
            LauncherError::repository(
                "repository.git-not-initialized",
                "该目录还不是 Git 仓库。请先运行 wwc init --confirm-git-init。",
            )
        })?;
        if choice == Some(BaselineChoice::Cancel) {
            return Ok(SetupOutcome::Cancelled {
                repository_root: display_path(root),
            });
        }
        let choice_required = inspection.head_sha.is_none() || !inspection.dirty_paths.is_empty();
        if choice_required && choice.is_none() {
            return Ok(SetupOutcome::BaselineChoiceRequired {
                repository_root: display_path(root),
                head_available: inspection.head_sha.is_some(),
                dirty_paths: inspection.dirty_paths,
                risk_warnings: inspection.risk_warnings,
                choices: baseline_choices(inspection.head_sha.is_some()),
            });
        }
        let selected = choice.unwrap_or(BaselineChoice::Head);
        let (baseline_sha, baseline_source, snapshot_ref) = match selected {
            BaselineChoice::Head => {
                let head = inspection.head_sha.clone().ok_or_else(|| {
                    LauncherError::repository(
                        "repository.head-missing",
                        "仓库还没有有效 HEAD，无法选择 HEAD；请选择 snapshot 或 cancel。",
                    )
                })?;
                (head, BaselineSource::Head, None)
            }
            BaselineChoice::Snapshot => {
                if !confirm_snapshot {
                    return Ok(SetupOutcome::SnapshotConfirmationRequired {
                        repository_root: display_path(root),
                        dirty_paths: inspection.dirty_paths,
                        risk_warnings: inspection.risk_warnings,
                    });
                }
                if !inspection.blocking_secret_paths.is_empty() {
                    return Err(LauncherError::repository(
                        "repository.snapshot-secret-blocked",
                        format!(
                            "Snapshot 中发现疑似秘密文件，已停止：{}。请移出、忽略或删除这些文件后重试。",
                            inspection.blocking_secret_paths.join(", ")
                        ),
                    ));
                }
                let snapshot = create_snapshot(
                    root,
                    inspection.head_sha.as_deref(),
                    &self.state_root.join("tmp"),
                )?;
                (
                    snapshot.commit_sha,
                    BaselineSource::SnapshotRef,
                    Some(snapshot.reference),
                )
            }
            BaselineChoice::Cancel => unreachable!("cancel is returned before baseline resolution"),
        };

        RepositoryContextScanner::default()
            .inspect(&RepositoryContextQuery::new(root, &baseline_sha))
            .map_err(|error| {
                LauncherError::repository(
                    "repository.context-failed",
                    format!("无法读取所选 baseline 的仓库信息：{error}"),
                )
            })?;
        let attachment = Attachment {
            schema_version: 1,
            repository_root: display_path(root),
            baseline_sha,
            baseline_source,
            snapshot_ref,
            remote_configured: inspection.remote_configured,
        };
        let outcome = self.persist_attachment(root, attachment)?;
        Ok(SetupOutcome::Ready {
            attachment: outcome,
        })
    }

    fn persist_attachment(
        &self,
        root: &Path,
        attachment: Attachment,
    ) -> Result<AttachmentOutcome, LauncherError> {
        let path = self.attachment_path(root);
        let bytes = serde_json::to_vec_pretty(&attachment).map_err(|error| {
            LauncherError::product(
                "product.attachment-encode-failed",
                format!("仓库绑定无法编码：{error}"),
            )
        })?;
        let mut canonical_bytes = bytes;
        canonical_bytes.push(b'\n');
        let state_changed = match fs::read(&path) {
            Ok(existing) if existing == canonical_bytes => false,
            Ok(_) | Err(_) => true,
        };
        if state_changed {
            let parent = path.parent().ok_or_else(|| {
                LauncherError::product(
                    "product.attachment-path-invalid",
                    "仓库绑定路径没有父目录。",
                )
            })?;
            fs::create_dir_all(parent).map_err(|error| {
                LauncherError::product(
                    "product.attachment-directory-failed",
                    format!("无法创建仓库绑定目录 {}：{error}", parent.display()),
                )
            })?;
            let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
            fs::write(&temporary, &canonical_bytes).map_err(|error| {
                LauncherError::product(
                    "product.attachment-write-failed",
                    format!("无法写入仓库绑定临时文件 {}：{error}", temporary.display()),
                )
            })?;
            fs::rename(&temporary, &path).map_err(|error| {
                let _ = fs::remove_file(&temporary);
                LauncherError::product(
                    "product.attachment-rename-failed",
                    format!("无法保存仓库绑定 {}：{error}", path.display()),
                )
            })?;
        }
        Ok(AttachmentOutcome {
            attachment,
            state_path: display_path(&path),
            state_changed,
        })
    }

    fn attachment_path(&self, root: &Path) -> PathBuf {
        let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
        self.state_root
            .join("repositories")
            .join(format!("{digest:x}.json"))
    }
}

impl LocalLauncherPort for SystemLocalLauncher {
    fn initialize_repository(&self, request: &InitRequest) -> Result<SetupOutcome, LauncherError> {
        let mut inspection = inspect_repository(&request.repository_path)?;
        if !inspection.git_initialized {
            if !request.confirm_git_init {
                return Ok(SetupOutcome::GitInitializationConfirmationRequired {
                    repository_path: display_path(&inspection.requested_path),
                });
            }
            initialize_git(&inspection.requested_path)?;
            inspection = inspect_repository(&inspection.requested_path)?;
        }
        self.setup_existing_repository(inspection, request.baseline, request.confirm_snapshot)
    }

    fn attach_repository(&self, request: &AttachRequest) -> Result<SetupOutcome, LauncherError> {
        let inspection = inspect_repository(&request.repository_path)?;
        self.setup_existing_repository(inspection, request.baseline, request.confirm_snapshot)
    }

    fn doctor(&self, request: &DoctorRequest) -> Result<DiagnosticReport, LauncherError> {
        Ok(build_diagnostic_report(self, request))
    }
}

/// Resolves the local state root without reading any credential values.
///
/// # Errors
///
/// Returns an environment error when neither `WINWINCODE_HOME` nor `HOME` is
/// available.
pub fn default_state_root() -> Result<PathBuf, LauncherError> {
    if let Some(path) = std::env::var_os("WINWINCODE_HOME") {
        return Ok(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".winwincode"))
        .ok_or_else(|| {
            LauncherError::environment(
                "environment.home-missing",
                "HOME 和 WINWINCODE_HOME 都没有设置。",
            )
        })
}

fn provider_variable_names() -> BTreeSet<String> {
    const NAMES: [&str; 5] = [
        "ANTHROPIC_API_KEY",
        "AZURE_OPENAI_API_KEY",
        "DEEPSEEK_API_KEY",
        "OPENAI_API_KEY",
        "WINWINCODE_PROVIDER",
    ];
    std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| NAMES.contains(&name.as_str()))
        .collect()
}

fn baseline_choices(head_available: bool) -> Vec<String> {
    let mut choices = Vec::new();
    if head_available {
        choices.push("head".to_owned());
    }
    choices.extend(["snapshot".to_owned(), "cancel".to_owned()]);
    choices
}

pub(crate) fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
