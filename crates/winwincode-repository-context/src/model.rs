use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};

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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactPath {
    pub path: String,
    pub evidence_path: String,
    pub rule: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactUnknown {
    pub capability: IndexCapability,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegressionSelection {
    pub affected_tests: Vec<String>,
    pub mandatory_commands: Vec<RepositoryCommand>,
    pub full_suite_required: bool,
    pub rationale: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryImpact {
    pub baseline_sha: String,
    pub direct: Vec<ImpactPath>,
    pub transitive: Vec<ImpactPath>,
    pub unknowns: Vec<ImpactUnknown>,
    pub regression: RegressionSelection,
}

impl RepositoryContext {
    /// Derives an explainable, conservative verification scope from candidate paths.
    ///
    /// File inventory can prove direct changes and same-package test relations. Missing
    /// graph capabilities remain explicit unknowns and force the full repository suite;
    /// they never remove repository-mandated test or verify commands.
    ///
    /// # Errors
    ///
    /// Rejects duplicate, absolute, or non-portable candidate paths.
    pub fn impact_of(
        &self,
        changed_paths: &[String],
    ) -> Result<RepositoryImpact, RepositoryContextError> {
        let changed = validated_changed_paths(changed_paths)?;
        let direct = changed
            .iter()
            .map(|path| ImpactPath {
                path: path.clone(),
                evidence_path: path.clone(),
                rule: "candidate-diff".to_owned(),
            })
            .collect::<Vec<_>>();
        let transitive = transitive_test_impact(self, &changed);
        let unknowns = impact_unknowns(self);
        let regression = regression_selection(self, &changed, &transitive, &unknowns);

        Ok(RepositoryImpact {
            baseline_sha: self.baseline_sha.clone(),
            direct,
            transitive,
            unknowns,
            regression,
        })
    }
}

fn transitive_test_impact(
    context: &RepositoryContext,
    changed: &BTreeSet<String>,
) -> Vec<ImpactPath> {
    let manifest_roots = context
        .package_managers
        .iter()
        .flat_map(|manager| manager.manifest_paths.iter())
        .filter_map(|manifest| Path::new(manifest).parent())
        .map(portable_path_text)
        .collect::<BTreeSet<_>>();
    let mut transitive = BTreeSet::new();
    for source in changed {
        let boundary = manifest_roots
            .iter()
            .filter(|root| within(root, source))
            .max_by_key(|root| root.len());
        let Some(boundary) = boundary else { continue };
        for test in &context.tests.test_files {
            if within(boundary, test) && !changed.contains(test) {
                transitive.insert((test.clone(), source.clone()));
            }
        }
    }
    transitive
        .into_iter()
        .map(|(path, evidence_path)| ImpactPath {
            path,
            evidence_path,
            rule: "same-package-test".to_owned(),
        })
        .collect()
}

fn impact_unknowns(context: &RepositoryContext) -> Vec<ImpactUnknown> {
    [
        IndexCapability::Callers,
        IndexCapability::Callees,
        IndexCapability::DependencyGraph,
        IndexCapability::TestRelations,
    ]
    .into_iter()
    .filter(|capability| !context.local_code_index.capabilities.supports(*capability))
    .map(|capability| ImpactUnknown {
        capability,
        reason: format!(
            "the {:?} index does not prove this relation",
            context.local_code_index.mode
        ),
    })
    .collect()
}

fn regression_selection(
    context: &RepositoryContext,
    changed: &BTreeSet<String>,
    transitive: &[ImpactPath],
    unknowns: &[ImpactUnknown],
) -> RegressionSelection {
    let affected_tests = context
        .tests
        .test_files
        .iter()
        .filter(|test| {
            changed.contains(*test) || transitive.iter().any(|impact| &impact.path == *test)
        })
        .cloned()
        .collect::<Vec<_>>();
    let mandatory_commands = context
        .commands
        .iter()
        .filter(|command| {
            matches!(
                command.purpose,
                CommandPurpose::Test | CommandPurpose::Verify
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    let high_risk_paths = changed
        .iter()
        .filter(|path| high_risk_path(path, context))
        .cloned()
        .collect::<Vec<_>>();
    let mut rationale = transitive
        .iter()
        .map(|impact| {
            format!(
                "rule=same-package-test source={} target={}",
                impact.evidence_path, impact.path
            )
        })
        .collect::<Vec<_>>();
    rationale.extend(
        unknowns
            .iter()
            .map(|unknown| format!("unknown={:?}", unknown.capability)),
    );
    rationale.extend(
        high_risk_paths
            .iter()
            .map(|path| format!("rule=high-risk-path source={path}")),
    );
    if mandatory_commands.is_empty() {
        rationale.push("unknown=mandatory-repository-test-command".to_owned());
    }
    RegressionSelection {
        affected_tests,
        mandatory_commands,
        full_suite_required: !unknowns.is_empty() || !high_risk_paths.is_empty(),
        rationale,
    }
}

fn validated_changed_paths(
    changed_paths: &[String],
) -> Result<BTreeSet<String>, RepositoryContextError> {
    if changed_paths.len() > 4096 {
        return Err(RepositoryContextError::InvalidChangedPath(
            "candidate path count exceeds 4096".to_owned(),
        ));
    }
    let mut result = BTreeSet::new();
    for path in changed_paths {
        let candidate = Path::new(path);
        if path.is_empty()
            || path.len() > 4096
            || path.contains(['\\', ':', '\0', '<', '>', '"', '|', '?', '*'])
            || path.bytes().any(|byte| byte.is_ascii_control())
            || path.split('/').any(invalid_portable_component)
            || candidate.is_absolute()
            || !candidate
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            || !result.insert(path.clone())
        {
            return Err(RepositoryContextError::InvalidChangedPath(path.clone()));
        }
    }
    Ok(result)
}

fn invalid_portable_component(component: &str) -> bool {
    if component.is_empty()
        || matches!(component, "." | "..")
        || component.eq_ignore_ascii_case(".git")
        || component.ends_with([' ', '.'])
    {
        return true;
    }
    let upper = component
        .split('.')
        .next()
        .unwrap_or(component)
        .to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

fn portable_path_text(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn within(root: &str, path: &str) -> bool {
    root.is_empty()
        || path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn high_risk_path(path: &str, context: &RepositoryContext) -> bool {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        file_name,
        "Cargo.lock"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "poetry.lock"
            | "uv.lock"
            | "requirements.txt"
            | "Cargo.toml"
            | "package.json"
    ) || [
        &context.paths.ci,
        &context.paths.migrations,
        &context.paths.deployment,
        &context.paths.security,
    ]
    .into_iter()
    .flatten()
    .any(|known| known == path)
}

#[derive(Debug)]
pub enum RepositoryContextError {
    InvalidBaselineSha(String),
    InvalidChangedPath(String),
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
            Self::InvalidChangedPath(path) => {
                write!(
                    formatter,
                    "candidate path is not portable and unique: {path}"
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
