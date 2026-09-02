use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use winwincode_repository_context::LocalCodeIndexSnapshot;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BaselineChoice {
    Head,
    Snapshot,
    Cancel,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitRequest {
    pub repository_path: PathBuf,
    pub confirm_git_init: bool,
    pub baseline: Option<BaselineChoice>,
    pub confirm_snapshot: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachRequest {
    pub repository_path: PathBuf,
    pub baseline: Option<BaselineChoice>,
    pub confirm_snapshot: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorRequest {
    pub repository_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BaselineSource {
    Head,
    SnapshotRef,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Attachment {
    pub schema_version: u8,
    pub repository_root: String,
    pub baseline_sha: String,
    pub baseline_source: BaselineSource,
    pub snapshot_ref: Option<String>,
    pub remote_configured: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentOutcome {
    pub attachment: Attachment,
    pub state_path: String,
    pub state_changed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum SetupOutcome {
    Ready {
        attachment: AttachmentOutcome,
    },
    GitInitializationConfirmationRequired {
        repository_path: String,
    },
    BaselineChoiceRequired {
        repository_root: String,
        head_available: bool,
        dirty_paths: Vec<String>,
        risk_warnings: Vec<String>,
        choices: Vec<String>,
    },
    SnapshotConfirmationRequired {
        repository_root: String,
        dirty_paths: Vec<String>,
        risk_warnings: Vec<String>,
    },
    Cancelled {
        repository_root: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryInspection {
    pub requested_path: PathBuf,
    pub repository_root: Option<PathBuf>,
    pub git_initialized: bool,
    pub head_sha: Option<String>,
    pub current_branch: Option<String>,
    pub dirty_paths: Vec<String>,
    pub risk_warnings: Vec<String>,
    pub blocking_secret_paths: Vec<String>,
    pub remote_configured: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticCategory {
    Product,
    Repository,
    Environment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticStatus {
    Pass,
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticCheck {
    pub category: DiagnosticCategory,
    pub status: DiagnosticStatus,
    pub code: String,
    pub message: String,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticReport {
    pub ok: bool,
    pub checks: Vec<DiagnosticCheck>,
    pub local_code_index: Option<LocalCodeIndexSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LauncherError {
    pub category: DiagnosticCategory,
    pub code: &'static str,
    pub message: String,
}

impl LauncherError {
    pub(crate) fn product(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            category: DiagnosticCategory::Product,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn repository(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            category: DiagnosticCategory::Repository,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn environment(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            category: DiagnosticCategory::Environment,
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for LauncherError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for LauncherError {}
