use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use crate::{
    CommandPurpose, GitRepositorySnapshot, IndexCapabilities, LanguageSummary, LocalCodeIndexMode,
    LocalCodeIndexProbe, LocalCodeIndexSnapshot, PackageManager, PackageManagerKind,
    RepositoryCommand, RepositoryContext, RepositoryContextError, RepositoryContextQuery,
    RepositoryFile, RepositoryPaths, TestContext,
};

pub trait RepositoryContextPort {
    /// Detects repository facts from the exact commit named by `query`.
    ///
    /// # Errors
    ///
    /// Returns an error when the baseline is not an exact reachable commit or
    /// a required baseline configuration file cannot be read.
    fn inspect(
        &self,
        query: &RepositoryContextQuery,
    ) -> Result<RepositoryContext, RepositoryContextError>;
}

pub trait LocalCodeIndexPort: Send + Sync {
    /// Reads the configured index status without changing repository files.
    ///
    /// # Errors
    ///
    /// Returns an error when the status provider cannot run or returns an
    /// invalid response.
    fn status(
        &self,
        repository_root: &Path,
        baseline_sha: &str,
    ) -> Result<LocalCodeIndexProbe, RepositoryContextError>;

    /// Refreshes only the external index cache for the requested baseline.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured index provider cannot refresh its
    /// cache. Implementations must not modify the target repository.
    fn refresh(
        &self,
        repository_root: &Path,
        baseline_sha: &str,
    ) -> Result<(), RepositoryContextError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FileInventoryLocalCodeIndex;

impl LocalCodeIndexPort for FileInventoryLocalCodeIndex {
    fn status(
        &self,
        _repository_root: &Path,
        baseline_sha: &str,
    ) -> Result<LocalCodeIndexProbe, RepositoryContextError> {
        Ok(LocalCodeIndexProbe {
            available: true,
            fresh: true,
            mode: LocalCodeIndexMode::GitFileInventory,
            baseline_sha: Some(baseline_sha.to_owned()),
            detail: "repository-local code-index command is not configured; inventory is read directly from the requested Git baseline".into(),
        })
    }

    fn refresh(
        &self,
        _repository_root: &Path,
        _baseline_sha: &str,
    ) -> Result<(), RepositoryContextError> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RepositoryContextScanner<I = FileInventoryLocalCodeIndex> {
    index: I,
}

impl Default for RepositoryContextScanner<FileInventoryLocalCodeIndex> {
    fn default() -> Self {
        Self {
            index: FileInventoryLocalCodeIndex,
        }
    }
}

impl<I> RepositoryContextScanner<I> {
    pub const fn new(index: I) -> Self {
        Self { index }
    }
}

impl<I: LocalCodeIndexPort> RepositoryContextPort for RepositoryContextScanner<I> {
    fn inspect(
        &self,
        query: &RepositoryContextQuery,
    ) -> Result<RepositoryContext, RepositoryContextError> {
        let snapshot = GitRepositorySnapshot::open(&query.repository_root, &query.baseline_sha)?;
        let baseline_sha = snapshot.baseline_sha().to_owned();
        let local_code_index =
            resolve_local_index(&self.index, &query.repository_root, &baseline_sha);
        let files = snapshot.files().to_vec();
        let project_files = files
            .iter()
            .filter(|file| !is_vendored_or_generated(&file.path))
            .collect::<Vec<_>>();

        Ok(RepositoryContext {
            baseline_sha,
            baseline_verified: true,
            languages: detect_languages(&project_files),
            package_managers: detect_package_managers(&project_files),
            commands: detect_commands(&snapshot, &project_files)?,
            paths: detect_repository_paths(&project_files),
            tests: detect_tests(&snapshot, &project_files),
            files,
            local_code_index,
        })
    }
}

fn resolve_local_index<I: LocalCodeIndexPort>(
    index: &I,
    repository_root: &Path,
    baseline_sha: &str,
) -> LocalCodeIndexSnapshot {
    let initial = index.status(repository_root, baseline_sha);
    if let Ok(probe) = &initial
        && is_verified_fresh(probe, baseline_sha)
    {
        return snapshot_from_probe(probe, baseline_sha, false);
    }

    let refresh = index.refresh(repository_root, baseline_sha);
    if refresh.is_ok()
        && let Ok(probe) = index.status(repository_root, baseline_sha)
        && is_verified_fresh(&probe, baseline_sha)
    {
        return snapshot_from_probe(&probe, baseline_sha, true);
    }

    let reason = match (initial, refresh) {
        (Err(status_error), Err(refresh_error)) => format!(
            "index status failed ({status_error}); refresh failed ({refresh_error}); using the requested baseline's Git file inventory"
        ),
        (Err(status_error), Ok(())) => format!(
            "index status failed ({status_error}); freshness remained unverifiable after refresh; using the requested baseline's Git file inventory"
        ),
        (Ok(probe), Err(refresh_error)) => format!(
            "index was not provably fresh ({detail}); refresh failed ({refresh_error}); using the requested baseline's Git file inventory",
            detail = probe.detail
        ),
        (Ok(probe), Ok(())) => format!(
            "index was not provably fresh ({detail}) after refresh; using the requested baseline's Git file inventory",
            detail = probe.detail
        ),
    };
    LocalCodeIndexSnapshot {
        available: true,
        fresh: true,
        mode: LocalCodeIndexMode::GitFileInventory,
        baseline_sha: baseline_sha.to_owned(),
        refresh_attempted: true,
        capabilities: IndexCapabilities::file_inventory(),
        detail: reason,
    }
}

fn is_verified_fresh(probe: &LocalCodeIndexProbe, baseline_sha: &str) -> bool {
    probe.available
        && probe.fresh
        && probe
            .baseline_sha
            .as_deref()
            .is_some_and(|indexed_sha| indexed_sha.eq_ignore_ascii_case(baseline_sha))
}

fn snapshot_from_probe(
    probe: &LocalCodeIndexProbe,
    baseline_sha: &str,
    refresh_attempted: bool,
) -> LocalCodeIndexSnapshot {
    let capabilities = match probe.mode {
        LocalCodeIndexMode::AstGrepOutline => IndexCapabilities::ast_grep_outline(),
        LocalCodeIndexMode::GitFileInventory => IndexCapabilities::file_inventory(),
    };
    LocalCodeIndexSnapshot {
        available: true,
        fresh: true,
        mode: probe.mode,
        baseline_sha: baseline_sha.to_owned(),
        refresh_attempted,
        capabilities,
        detail: probe.detail.clone(),
    }
}

fn detect_languages(files: &[&RepositoryFile]) -> Vec<LanguageSummary> {
    let mut languages = BTreeMap::<String, (usize, Vec<String>)>::new();
    for file in files {
        let Some(language) = language_for_path(&file.path) else {
            continue;
        };
        let entry = languages.entry(language.to_owned()).or_default();
        entry.0 += 1;
        if entry.1.len() < 5 {
            entry.1.push(file.path.clone());
        }
    }
    let mut result = languages
        .into_iter()
        .map(|(language, (file_count, evidence_paths))| LanguageSummary {
            language,
            file_count,
            evidence_paths,
        })
        .collect::<Vec<_>>();
    result.sort_by(|left, right| {
        right
            .file_count
            .cmp(&left.file_count)
            .then_with(|| left.language.cmp(&right.language))
    });
    result
}

fn language_for_path(path: &str) -> Option<&'static str> {
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "c" | "h" => Some("C"),
        "cc" | "cpp" | "cxx" | "hpp" => Some("C++"),
        "cs" => Some("C#"),
        "css" => Some("CSS"),
        "go" => Some("Go"),
        "html" => Some("HTML"),
        "java" => Some("Java"),
        "js" | "jsx" | "mjs" | "cjs" => Some("JavaScript"),
        "json" => Some("JSON"),
        "kt" | "kts" => Some("Kotlin"),
        "md" | "mdx" => Some("Markdown"),
        "php" => Some("PHP"),
        "py" => Some("Python"),
        "rb" => Some("Ruby"),
        "rs" => Some("Rust"),
        "sh" | "bash" | "zsh" => Some("Shell"),
        "sql" => Some("SQL"),
        "swift" => Some("Swift"),
        "toml" => Some("TOML"),
        "ts" | "tsx" | "mts" | "cts" => Some("TypeScript"),
        "vue" => Some("Vue"),
        "xml" => Some("XML"),
        "yaml" | "yml" => Some("YAML"),
        _ => None,
    }
}

fn detect_package_managers(files: &[&RepositoryFile]) -> Vec<PackageManager> {
    let paths = files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    let mut result = Vec::new();
    add_manager(
        &mut result,
        &paths,
        PackageManagerKind::Cargo,
        &["Cargo.toml"],
        &["Cargo.lock"],
    );
    if paths.contains("pnpm-lock.yaml") {
        add_manager(
            &mut result,
            &paths,
            PackageManagerKind::Pnpm,
            &["package.json"],
            &["pnpm-lock.yaml"],
        );
    } else if paths.contains("yarn.lock") {
        add_manager(
            &mut result,
            &paths,
            PackageManagerKind::Yarn,
            &["package.json"],
            &["yarn.lock"],
        );
    } else if paths.contains("bun.lock") || paths.contains("bun.lockb") {
        add_manager(
            &mut result,
            &paths,
            PackageManagerKind::Bun,
            &["package.json"],
            &["bun.lock", "bun.lockb"],
        );
    } else if paths.contains("package-lock.json") {
        add_manager(
            &mut result,
            &paths,
            PackageManagerKind::Npm,
            &["package.json"],
            &["package-lock.json"],
        );
    }
    add_manager(
        &mut result,
        &paths,
        PackageManagerKind::GoModules,
        &["go.mod"],
        &["go.sum"],
    );
    add_manager(
        &mut result,
        &paths,
        PackageManagerKind::Poetry,
        &["pyproject.toml"],
        &["poetry.lock"],
    );
    add_manager(
        &mut result,
        &paths,
        PackageManagerKind::Uv,
        &["pyproject.toml"],
        &["uv.lock"],
    );
    if !paths.contains("poetry.lock") && !paths.contains("uv.lock") {
        add_manager(
            &mut result,
            &paths,
            PackageManagerKind::Pip,
            &["requirements.txt", "pyproject.toml"],
            &[],
        );
    }
    add_manager(
        &mut result,
        &paths,
        PackageManagerKind::Maven,
        &["pom.xml"],
        &[],
    );
    add_manager(
        &mut result,
        &paths,
        PackageManagerKind::Gradle,
        &["build.gradle", "build.gradle.kts"],
        &["gradle.lockfile"],
    );
    result
}

fn add_manager(
    result: &mut Vec<PackageManager>,
    paths: &BTreeSet<&str>,
    kind: PackageManagerKind,
    manifest_names: &[&str],
    lock_names: &[&str],
) {
    let manifest_paths = paths
        .iter()
        .filter(|path| manifest_names.iter().any(|name| file_name(path) == *name))
        .map(|path| (*path).to_owned())
        .collect::<Vec<_>>();
    if manifest_paths.is_empty() {
        return;
    }
    let lockfile = lock_names
        .iter()
        .find_map(|name| paths.get(name).map(|path| (*path).to_owned()));
    result.push(PackageManager {
        kind,
        lockfile,
        manifest_paths,
    });
}

fn detect_commands(
    snapshot: &GitRepositorySnapshot,
    files: &[&RepositoryFile],
) -> Result<Vec<RepositoryCommand>, RepositoryContextError> {
    let paths = files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    let mut commands = Vec::new();
    if paths.contains("Cargo.toml") {
        commands.extend([
            command(
                CommandPurpose::Build,
                "cargo build --workspace --locked",
                "Cargo.toml",
            ),
            command(
                CommandPurpose::Test,
                "cargo test --workspace --locked",
                "Cargo.toml",
            ),
            command(
                CommandPurpose::Format,
                "cargo fmt --all -- --check",
                "Cargo.toml",
            ),
            command(
                CommandPurpose::StaticAnalysis,
                "cargo clippy --workspace --all-targets --locked -- -D warnings",
                "Cargo.toml",
            ),
        ]);
    }

    let js_manager = if paths.contains("pnpm-lock.yaml") {
        Some("corepack pnpm")
    } else if paths.contains("yarn.lock") {
        Some("corepack yarn")
    } else if paths.contains("bun.lock") || paths.contains("bun.lockb") {
        Some("bun")
    } else if paths.contains("package-lock.json") {
        Some("npm")
    } else {
        None
    };
    if let Some(manager) = js_manager {
        for package_json in paths
            .iter()
            .filter(|path| file_name(path) == "package.json")
        {
            let content = snapshot.read_text(package_json)?;
            let value: Value = serde_json::from_str(&content).map_err(|error| {
                RepositoryContextError::SnapshotRead {
                    path: (*package_json).to_owned(),
                    detail: error.to_string(),
                }
            })?;
            let Some(scripts) = value.get("scripts").and_then(Value::as_object) else {
                continue;
            };
            let package_dir = Path::new(package_json)
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .and_then(Path::to_str);
            for script in scripts.keys() {
                let Some(purpose) = purpose_for_script(script) else {
                    continue;
                };
                let invocation = match package_dir {
                    Some(directory) => format!("{manager} --dir {directory} run {script}"),
                    None => format!("{manager} run {script}"),
                };
                commands.push(command(purpose, &invocation, package_json));
            }
        }
    }

    commands.sort_by(|left, right| {
        left.purpose
            .cmp(&right.purpose)
            .then_with(|| left.command.cmp(&right.command))
    });
    commands.dedup_by(|left, right| left.command == right.command);
    Ok(commands)
}

fn command(purpose: CommandPurpose, command: &str, evidence_path: &str) -> RepositoryCommand {
    RepositoryCommand {
        purpose,
        command: command.to_owned(),
        evidence_path: evidence_path.to_owned(),
    }
}

fn purpose_for_script(script: &str) -> Option<CommandPurpose> {
    let normalized = script.to_ascii_lowercase();
    if normalized == "build" || normalized.starts_with("build:") {
        Some(CommandPurpose::Build)
    } else if normalized == "test" || normalized.starts_with("test:") {
        Some(CommandPurpose::Test)
    } else if matches!(
        normalized.as_str(),
        "typecheck" | "type-check" | "check-types"
    ) || normalized.starts_with("typecheck:")
    {
        Some(CommandPurpose::TypeCheck)
    } else if normalized == "lint" || normalized.starts_with("lint:") {
        Some(CommandPurpose::Lint)
    } else if normalized == "format" || normalized == "fmt" || normalized.starts_with("format:") {
        Some(CommandPurpose::Format)
    } else if normalized == "verify" || normalized.starts_with("verify:") {
        Some(CommandPurpose::Verify)
    } else if normalized == "audit" || normalized.starts_with("audit:") || normalized == "analyze" {
        Some(CommandPurpose::StaticAnalysis)
    } else {
        None
    }
}

fn detect_repository_paths(files: &[&RepositoryFile]) -> RepositoryPaths {
    let mut paths = RepositoryPaths::default();
    for file in files {
        let path = file.path.as_str();
        if is_ci_path(path) {
            paths.ci.push(path.to_owned());
        }
        if has_segment(path, &["migration", "migrations", "migrate"]) {
            paths.migrations.push(path.to_owned());
        }
        if is_deployment_path(path) {
            paths.deployment.push(path.to_owned());
        }
        if is_security_path(path) {
            paths.security.push(path.to_owned());
        }
        if is_agent_instruction(path) {
            paths.agent_instructions.push(path.to_owned());
        }
    }
    paths.ci.sort();
    paths.migrations.sort();
    paths.deployment.sort();
    paths.security.sort();
    paths.agent_instructions.sort();
    paths
}

fn detect_tests(snapshot: &GitRepositorySnapshot, files: &[&RepositoryFile]) -> TestContext {
    let mut context = TestContext::default();
    let mut roots = BTreeSet::new();
    let mut runners = BTreeSet::new();
    for file in files {
        let path = file.path.as_str();
        if is_test_file(path) {
            context.test_files.push(path.to_owned());
            if let Some(root) = test_root(path) {
                roots.insert(root);
            }
        }
        if has_segment(path, &["fixture", "fixtures", "testdata", "test-data"]) {
            context.fixture_paths.push(path.to_owned());
        }
        if has_segment(path, &["mock", "mocks", "__mocks__"]) {
            context.mock_paths.push(path.to_owned());
        }
        if has_segment(path, &["snapshot", "snapshots", "__snapshots__"])
            || Path::new(path)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("snap"))
        {
            context.snapshot_paths.push(path.to_owned());
        }
        detect_runner_from_path(path, &mut runners);
    }
    if snapshot
        .files()
        .iter()
        .any(|file| file.path == "package.json")
        && let Ok(package_json) = snapshot.read_text("package.json")
    {
        detect_js_runners(&package_json, &mut runners);
    }
    context.test_roots = roots.into_iter().collect();
    context.runners = runners.into_iter().collect();
    context
}

fn is_test_file(path: &str) -> bool {
    has_segment(path, &["test", "tests", "__tests__", "e2e"])
        || path.ends_with("_test.rs")
        || path.ends_with(".test.ts")
        || path.ends_with(".test.tsx")
        || path.ends_with(".test.js")
        || path.ends_with(".test.mjs")
        || path.ends_with(".spec.ts")
        || path.ends_with(".spec.tsx")
        || path.ends_with(".spec.js")
        || path.ends_with(".spec.mjs")
        || path.ends_with("_test.py")
        || path.starts_with("test_")
}

fn test_root(path: &str) -> Option<String> {
    let mut current = String::new();
    for segment in path.split('/') {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(segment);
        if matches!(segment, "test" | "tests" | "__tests__" | "e2e") {
            return Some(current);
        }
    }
    Path::new(path)
        .parent()
        .and_then(Path::to_str)
        .map(str::to_owned)
}

fn detect_runner_from_path(path: &str, runners: &mut BTreeSet<String>) {
    let name = file_name(path).to_ascii_lowercase();
    let detected = match name.as_str() {
        "cargo.toml" => Some("cargo-test"),
        "jest.config.js" | "jest.config.ts" | "jest.config.mjs" => Some("jest"),
        "vitest.config.js" | "vitest.config.ts" | "vitest.config.mjs" => Some("vitest"),
        "playwright.config.js" | "playwright.config.ts" => Some("playwright"),
        "pytest.ini" | "conftest.py" => Some("pytest"),
        _ => None,
    };
    if let Some(detected) = detected {
        runners.insert(detected.to_owned());
    }
}

fn detect_js_runners(package_json: &str, runners: &mut BTreeSet<String>) {
    let Ok(value) = serde_json::from_str::<Value>(package_json) else {
        return;
    };
    for section in ["dependencies", "devDependencies"] {
        let Some(dependencies) = value.get(section).and_then(Value::as_object) else {
            continue;
        };
        for (package, runner) in [
            ("jest", "jest"),
            ("vitest", "vitest"),
            ("@playwright/test", "playwright"),
            ("mocha", "mocha"),
            ("ava", "ava"),
        ] {
            if dependencies.contains_key(package) {
                runners.insert(runner.to_owned());
            }
        }
    }
}

fn is_vendored_or_generated(path: &str) -> bool {
    has_segment(
        path,
        &[
            "node_modules",
            "target",
            "vendor",
            "third_party",
            "generated",
        ],
    )
}

fn is_ci_path(path: &str) -> bool {
    path.starts_with(".github/workflows/")
        || path == ".gitlab-ci.yml"
        || path == "Jenkinsfile"
        || path == "azure-pipelines.yml"
        || path == ".circleci/config.yml"
}

fn is_deployment_path(path: &str) -> bool {
    let name = file_name(path).to_ascii_lowercase();
    has_segment(
        path,
        &[
            "deploy",
            "deployment",
            "deployments",
            "helm",
            "k8s",
            "kubernetes",
            "terraform",
            "infra",
        ],
    ) || name == "dockerfile"
        || name.starts_with("docker-compose")
        || name.starts_with("compose.")
}

fn is_security_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    has_segment(path, &["security", "secrets", "policies", "policy"])
        || matches!(
            lower.as_str(),
            "security.md"
                | ".snyk"
                | "codeowners"
                | ".github/codeowners"
                | ".github/dependabot.yml"
                | ".github/dependabot.yaml"
        )
}

fn is_agent_instruction(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    matches!(
        file_name(&lower).as_str(),
        "agents.md" | "claude.md" | "gemini.md" | ".cursorrules" | "copilot-instructions.md"
    ) || lower.starts_with(".github/instructions/") && lower.ends_with(".instructions.md")
}

fn has_segment(path: &str, expected: &[&str]) -> bool {
    path.split('/').any(|segment| {
        expected
            .iter()
            .any(|candidate| segment.eq_ignore_ascii_case(candidate))
    })
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_owned()
}
