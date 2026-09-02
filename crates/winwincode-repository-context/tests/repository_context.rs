use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use winwincode_repository_context::{
    FileInventoryLocalCodeIndex, IndexCapability, LocalCodeIndexMode, LocalCodeIndexPort,
    LocalCodeIndexProbe, PackageManagerKind, RepositoryContextError, RepositoryContextPort,
    RepositoryContextQuery, RepositoryContextScanner,
};

#[test]
fn detects_repository_facts_from_the_exact_baseline() {
    let repository = FixtureRepository::new();
    repository.write(
        "package.json",
        r#"{
          "scripts": {
            "build": "tsc",
            "test": "node --test",
            "typecheck": "tsc --noEmit",
            "lint": "eslint .",
            "verify": "pnpm test"
          },
          "devDependencies": { "@playwright/test": "1.0.0" }
        }"#,
    );
    repository.write("pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
    repository.write("Cargo.toml", "[workspace]\nmembers = []\n");
    repository.write("Cargo.lock", "version = 4\n");
    repository.write("src/lib.rs", "pub fn answer() -> u8 { 42 }\n");
    repository.write("apps/web/src/main.ts", "export const answer = 42;\n");
    repository.write("tests/api.test.mjs", "// test\n");
    repository.write("tests/fixtures/input.json", "{}\n");
    repository.write("tests/__snapshots__/api.snap", "snapshot\n");
    repository.write(".github/workflows/verify.yml", "name: verify\n");
    repository.write("db/migrations/001.sql", "select 1;\n");
    repository.write("deploy/Dockerfile", "FROM scratch\n");
    repository.write("SECURITY.md", "# Security\n");
    repository.write("AGENTS.md", "# Instructions\n");
    let baseline = repository.commit();

    let context = RepositoryContextScanner::default()
        .inspect(&RepositoryContextQuery::new(repository.path(), &baseline))
        .expect("repository context should be detected");

    assert_eq!(context.baseline_sha, baseline);
    assert!(context.baseline_verified);
    assert!(context.languages.iter().any(|item| item.language == "Rust"));
    assert!(
        context
            .languages
            .iter()
            .any(|item| item.language == "TypeScript")
    );
    assert!(
        context
            .package_managers
            .iter()
            .any(|manager| manager.kind == PackageManagerKind::Cargo)
    );
    assert!(
        context
            .package_managers
            .iter()
            .any(|manager| manager.kind == PackageManagerKind::Pnpm)
    );
    assert!(
        context
            .commands
            .iter()
            .any(|item| item.command == "corepack pnpm run typecheck")
    );
    assert_eq!(context.paths.ci, [".github/workflows/verify.yml"]);
    assert_eq!(context.paths.migrations, ["db/migrations/001.sql"]);
    assert_eq!(context.paths.agent_instructions, ["AGENTS.md"]);
    assert!(context.tests.runners.contains(&"playwright".to_owned()));
    assert!(
        context
            .tests
            .fixture_paths
            .contains(&"tests/fixtures/input.json".to_owned())
    );
    assert!(context.local_code_index.available);
    assert!(context.local_code_index.fresh);
    assert_eq!(
        context.local_code_index.mode,
        LocalCodeIndexMode::GitFileInventory
    );
    assert!(
        !context
            .local_code_index
            .capabilities
            .supports(IndexCapability::SymbolOutlines)
    );
    assert!(
        !context
            .local_code_index
            .capabilities
            .supports(IndexCapability::Callers)
    );
    assert!(
        !context
            .local_code_index
            .capabilities
            .supports(IndexCapability::DependencyGraph)
    );
}

#[test]
fn ignores_dirty_worktree_changes_after_the_baseline() {
    let repository = FixtureRepository::new();
    repository.write("package.json", r#"{"scripts":{"test":"node --test"}}"#);
    repository.write("pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
    let baseline = repository.commit();
    repository.write(
        "package.json",
        r#"{"scripts":{"test":"node --test","deploy":"danger"}}"#,
    );
    repository.write("new-untracked.py", "print('not in baseline')\n");

    let context = RepositoryContextScanner::new(FileInventoryLocalCodeIndex)
        .inspect(&RepositoryContextQuery::new(repository.path(), &baseline))
        .expect("baseline should remain readable");

    assert!(
        !context
            .files
            .iter()
            .any(|file| file.path == "new-untracked.py")
    );
    assert!(
        !context
            .commands
            .iter()
            .any(|item| item.command.contains("deploy"))
    );
}

#[test]
fn rejects_symbolic_or_missing_baselines() {
    let repository = FixtureRepository::new();
    repository.write("README.md", "fixture\n");
    let baseline = repository.commit();
    let scanner = RepositoryContextScanner::default();

    assert!(matches!(
        scanner.inspect(&RepositoryContextQuery::new(repository.path(), "HEAD")),
        Err(RepositoryContextError::InvalidBaselineSha(_))
    ));
    let missing = "0".repeat(baseline.len());
    assert!(matches!(
        scanner.inspect(&RepositoryContextQuery::new(repository.path(), missing)),
        Err(RepositoryContextError::BaselineNotFound(_))
    ));
}

#[test]
fn refreshes_a_stale_index_then_reports_verified_symbol_coverage() {
    let repository = FixtureRepository::new();
    repository.write("src/lib.rs", "pub fn ready() {}\n");
    let baseline = repository.commit();
    let scanner = RepositoryContextScanner::new(FakeIndex::stale_then_fresh(&baseline));

    let context = scanner
        .inspect(&RepositoryContextQuery::new(repository.path(), &baseline))
        .expect("refresh should make the index usable");

    assert_eq!(
        context.local_code_index.mode,
        LocalCodeIndexMode::AstGrepOutline
    );
    assert!(context.local_code_index.refresh_attempted);
    assert!(
        context
            .local_code_index
            .capabilities
            .supports(IndexCapability::SymbolOutlines)
    );
    assert!(
        !context
            .local_code_index
            .capabilities
            .supports(IndexCapability::Callers)
    );
    assert!(
        !context
            .local_code_index
            .capabilities
            .supports(IndexCapability::Callees)
    );
}

#[test]
fn falls_back_to_file_coverage_when_freshness_cannot_be_proved() {
    let repository = FixtureRepository::new();
    repository.write("src/lib.rs", "pub fn ready() {}\n");
    let baseline = repository.commit();
    let scanner = RepositoryContextScanner::new(FakeIndex::wrong_baseline());

    let context = scanner
        .inspect(&RepositoryContextQuery::new(repository.path(), &baseline))
        .expect("repository facts should remain available");

    assert_eq!(
        context.local_code_index.mode,
        LocalCodeIndexMode::GitFileInventory
    );
    assert!(context.local_code_index.fresh);
    assert_eq!(context.local_code_index.baseline_sha, baseline);
    assert!(context.local_code_index.refresh_attempted);
    assert!(
        context
            .local_code_index
            .capabilities
            .supports(IndexCapability::FilePaths)
    );
    assert!(
        !context
            .local_code_index
            .capabilities
            .supports(IndexCapability::SymbolOutlines)
    );
    assert!(
        !context
            .local_code_index
            .capabilities
            .supports(IndexCapability::Callers)
    );
    assert!(
        !context
            .local_code_index
            .capabilities
            .supports(IndexCapability::TestRelations)
    );
    assert!(
        context
            .local_code_index
            .detail
            .contains("not provably fresh")
    );
}

struct FakeIndex {
    state: Mutex<FakeIndexState>,
}

struct FakeIndexState {
    baseline: String,
    become_fresh: bool,
    refreshed: bool,
}

impl FakeIndex {
    fn stale_then_fresh(baseline: &str) -> Self {
        Self {
            state: Mutex::new(FakeIndexState {
                baseline: baseline.to_owned(),
                become_fresh: true,
                refreshed: false,
            }),
        }
    }

    fn wrong_baseline() -> Self {
        Self {
            state: Mutex::new(FakeIndexState {
                baseline: "f".repeat(40),
                become_fresh: false,
                refreshed: false,
            }),
        }
    }
}

impl LocalCodeIndexPort for FakeIndex {
    fn status(
        &self,
        _repository_root: &Path,
        _baseline_sha: &str,
    ) -> Result<LocalCodeIndexProbe, RepositoryContextError> {
        let state = self.state.lock().expect("fake index mutex");
        Ok(LocalCodeIndexProbe {
            available: true,
            fresh: state.refreshed && state.become_fresh,
            mode: LocalCodeIndexMode::AstGrepOutline,
            baseline_sha: Some(state.baseline.clone()),
            detail: "fixture index".into(),
        })
    }

    fn refresh(
        &self,
        _repository_root: &Path,
        _baseline_sha: &str,
    ) -> Result<(), RepositoryContextError> {
        self.state.lock().expect("fake index mutex").refreshed = true;
        Ok(())
    }
}

struct FixtureRepository {
    path: PathBuf,
}

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl FixtureRepository {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-repository-context-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("fixture root should be created");
        run_git(&path, &["init", "--quiet"]);
        run_git(&path, &["config", "user.name", "WinWinCode Test"]);
        run_git(&path, &["config", "user.email", "test@winwincode.invalid"]);
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, relative_path: &str, content: &str) {
        let path = self.path.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent should be created");
        }
        fs::write(path, content).expect("fixture file should be written");
    }

    fn commit(&self) -> String {
        run_git(&self.path, &["add", "."]);
        run_git(&self.path, &["commit", "--quiet", "-m", "fixture baseline"]);
        String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&self.path)
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("git rev-parse should run")
                .stdout,
        )
        .expect("SHA should be UTF-8")
        .trim()
        .to_owned()
    }
}

impl Drop for FixtureRepository {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run_git(root: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .expect("git should run");
    assert!(
        output.status.success(),
        "git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
