use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryContextQuery {
    pub repository_root: PathBuf,
    pub baseline_sha: String,
}

impl RepositoryContextQuery {
    pub fn new(repository_root: impl Into<PathBuf>, baseline_sha: impl Into<String>) -> Self {
        Self {
            repository_root: repository_root.into(),
            baseline_sha: baseline_sha.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalCodeIndexMode {
    AstGrepOutline,
    GitFileInventory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IndexCapability {
    FilePaths,
    Languages,
    Sizes,
    ContentFingerprints,
    SymbolOutlines,
    Callers,
    Callees,
    DependencyGraph,
    TestRelations,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexCapabilities {
    pub supported: Vec<IndexCapability>,
}

impl IndexCapabilities {
    #[must_use]
    pub fn ast_grep_outline() -> Self {
        Self {
            supported: vec![
                IndexCapability::FilePaths,
                IndexCapability::Languages,
                IndexCapability::Sizes,
                IndexCapability::ContentFingerprints,
                IndexCapability::SymbolOutlines,
            ],
        }
    }

    #[must_use]
    pub fn file_inventory() -> Self {
        Self {
            supported: vec![
                IndexCapability::FilePaths,
                IndexCapability::Languages,
                IndexCapability::Sizes,
                IndexCapability::ContentFingerprints,
            ],
        }
    }

    #[must_use]
    pub fn supports(&self, capability: IndexCapability) -> bool {
        self.supported.contains(&capability)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCodeIndexProbe {
    pub available: bool,
    pub fresh: bool,
    pub mode: LocalCodeIndexMode,
    pub baseline_sha: Option<String>,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCodeIndexSnapshot {
    pub available: bool,
    pub fresh: bool,
    pub mode: LocalCodeIndexMode,
    pub baseline_sha: String,
    pub refresh_attempted: bool,
    pub capabilities: IndexCapabilities,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryFile {
    pub path: String,
    pub bytes: Option<u64>,
    pub content_fingerprint: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanguageSummary {
    pub language: String,
    pub file_count: usize,
    pub evidence_paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackageManagerKind {
    Cargo,
    Pnpm,
    Npm,
    Yarn,
    Bun,
    GoModules,
    Poetry,
    Uv,
    Pip,
    Maven,
    Gradle,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManager {
    pub kind: PackageManagerKind,
    pub lockfile: Option<String>,
    pub manifest_paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandPurpose {
    Build,
    Format,
    Lint,
    StaticAnalysis,
    Test,
    TypeCheck,
    Verify,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryCommand {
    pub purpose: CommandPurpose,
    pub command: String,
    pub evidence_path: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryPaths {
    pub ci: Vec<String>,
    pub migrations: Vec<String>,
    pub deployment: Vec<String>,
    pub security: Vec<String>,
    pub agent_instructions: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestContext {
    pub test_files: Vec<String>,
    pub test_roots: Vec<String>,
    pub fixture_paths: Vec<String>,
    pub mock_paths: Vec<String>,
    pub snapshot_paths: Vec<String>,
    pub runners: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryContext {
    pub baseline_sha: String,
    pub baseline_verified: bool,
    pub files: Vec<RepositoryFile>,
    pub languages: Vec<LanguageSummary>,
    pub package_managers: Vec<PackageManager>,
    pub commands: Vec<RepositoryCommand>,
    pub paths: RepositoryPaths,
    pub tests: TestContext,
    pub local_code_index: LocalCodeIndexSnapshot,
}

#[derive(Debug)]
pub enum RepositoryContextError {
    InvalidBaselineSha(String),
    BaselineNotFound(String),
    GitCommand {
        operation: &'static str,
        detail: String,
    },
    SnapshotRead {
        path: String,
        detail: String,
    },
    IndexCommand(String),
    IndexResponse(String),
}

impl fmt::Display for RepositoryContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBaselineSha(sha) => {
                write!(
                    formatter,
                    "baseline must be an exact 40- or 64-digit Git SHA: {sha}"
                )
            }
            Self::BaselineNotFound(sha) => {
                write!(formatter, "baseline commit does not exist: {sha}")
            }
            Self::GitCommand { operation, detail } => {
                write!(formatter, "Git {operation} failed: {detail}")
            }
            Self::SnapshotRead { path, detail } => {
                write!(
                    formatter,
                    "failed to read {path} from the baseline: {detail}"
                )
            }
            Self::IndexCommand(detail) => {
                write!(formatter, "local code-index command failed: {detail}")
            }
            Self::IndexResponse(detail) => {
                write!(formatter, "local code-index status is invalid: {detail}")
            }
        }
    }
}

impl std::error::Error for RepositoryContextError {}
