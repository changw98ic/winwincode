use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use winwincode_cli::{LocalLauncherPort, SystemLocalLauncher, render_help, run_cli};
use winwincode_repository_context::{IndexCapability, LocalCodeIndexMode};

#[test]
fn help_exposes_one_canonical_wwc_path() {
    let help = render_help();
    assert!(help.contains("wwc init"));
    assert!(help.contains("wwc repo attach"));
    assert!(help.contains("wwc doctor"));
    assert!(!help.contains("winwincode init"));
}

#[test]
fn git_initialization_and_snapshot_both_require_explicit_confirmation() {
    let fixture = Fixture::new();
    fixture.write("repo/src/lib.rs", "pub fn ready() {}\n");
    let launcher = fixture.launcher();

    let before = invoke(&launcher, &["init", fixture.repo_str(), "--json"]);
    assert_eq!(before.code, 3);
    assert_eq!(
        json(&before.stdout)["status"],
        "git-initialization-confirmation-required"
    );
    assert!(!fixture.repo().join(".git").exists());

    let after_init = invoke(
        &launcher,
        &["init", fixture.repo_str(), "--confirm-git-init", "--json"],
    );
    assert_eq!(after_init.code, 3);
    assert_eq!(
        json(&after_init.stdout)["status"],
        "baseline-choice-required"
    );
    assert!(fixture.repo().join(".git").is_dir());
    assert_eq!(json(&after_init.stdout)["headAvailable"], false);

    let before_snapshot = invoke(
        &launcher,
        &[
            "init",
            fixture.repo_str(),
            "--baseline",
            "snapshot",
            "--json",
        ],
    );
    assert_eq!(before_snapshot.code, 3);
    assert_eq!(
        json(&before_snapshot.stdout)["status"],
        "snapshot-confirmation-required"
    );
    assert!(
        git(
            fixture.repo(),
            &["for-each-ref", "refs/winwincode/snapshots"]
        )
        .is_empty()
    );

    let ready = invoke(
        &launcher,
        &[
            "init",
            fixture.repo_str(),
            "--baseline",
            "snapshot",
            "--confirm-snapshot",
            "--json",
        ],
    );
    assert_eq!(ready.code, 0, "{}", ready.stderr);
    let ready_json = json(&ready.stdout);
    assert_eq!(ready_json["status"], "ready");
    assert_eq!(
        ready_json["attachment"]["attachment"]["baselineSource"],
        "snapshot-ref"
    );
    assert_eq!(ready_json["attachment"]["stateChanged"], true);
    assert!(
        ready_json["attachment"]["attachment"]["snapshotRef"]
            .as_str()
            .is_some_and(|value| value.starts_with("refs/winwincode/snapshots/"))
    );

    let repeated = invoke(
        &launcher,
        &[
            "init",
            fixture.repo_str(),
            "--baseline",
            "snapshot",
            "--confirm-snapshot",
            "--json",
        ],
    );
    assert_eq!(repeated.code, 0);
    let repeated_json = json(&repeated.stdout);
    assert_eq!(repeated_json["attachment"]["stateChanged"], false);
    assert_eq!(
        repeated_json["attachment"]["attachment"]["baselineSha"],
        ready_json["attachment"]["attachment"]["baselineSha"]
    );
    assert_snapshot_doctor(&launcher, fixture.repo(), &ready_json);
}

#[test]
fn dirty_repository_requires_head_snapshot_or_cancel_and_head_is_idempotent() {
    let fixture = Fixture::new();
    fixture.init_git();
    fixture.write("repo/src/lib.rs", "pub fn baseline() {}\n");
    let head = fixture.commit();
    fixture.write("repo/src/lib.rs", "pub fn dirty() {}\n");
    fixture.write("repo/tests/new.rs", "#[test] fn new_test() {}\n");
    let launcher = fixture.launcher();

    let choice = invoke(&launcher, &["repo", "attach", fixture.repo_str(), "--json"]);
    assert_eq!(choice.code, 3);
    let choice_json = json(&choice.stdout);
    assert_eq!(choice_json["status"], "baseline-choice-required");
    assert_eq!(choice_json["headAvailable"], true);
    assert_eq!(
        choice_json["choices"],
        serde_json::json!(["head", "snapshot", "cancel"])
    );

    let cancelled = invoke(
        &launcher,
        &[
            "repo",
            "attach",
            fixture.repo_str(),
            "--baseline",
            "cancel",
            "--json",
        ],
    );
    assert_eq!(cancelled.code, 0);
    assert_eq!(json(&cancelled.stdout)["status"], "cancelled");
    assert!(!fixture.state().join("repositories").exists());

    let attached = invoke(
        &launcher,
        &[
            "repo",
            "attach",
            fixture.repo_str(),
            "--baseline",
            "head",
            "--json",
        ],
    );
    assert_eq!(attached.code, 0, "{}", attached.stderr);
    let attached_json = json(&attached.stdout);
    assert_eq!(
        attached_json["attachment"]["attachment"]["baselineSha"],
        head
    );
    assert_eq!(
        attached_json["attachment"]["attachment"]["remoteConfigured"],
        false
    );
    assert_eq!(attached_json["attachment"]["stateChanged"], true);

    let repeated = invoke(
        &launcher,
        &[
            "repo",
            "attach",
            fixture.repo_str(),
            "--baseline",
            "head",
            "--json",
        ],
    );
    assert_eq!(json(&repeated.stdout)["attachment"]["stateChanged"], false);
}

#[test]
fn snapshot_ref_does_not_change_branch_index_worktree_or_stash() {
    let fixture = Fixture::new();
    fixture.init_git();
    fixture.write("repo/src/lib.rs", "pub fn baseline() {}\n");
    let head = fixture.commit();
    fixture.write("repo/src/lib.rs", "pub fn changed() {}\n");
    fixture.write("repo/new.txt", "new\n");
    let launcher = fixture.launcher();
    let branch_before = git(fixture.repo(), &["symbolic-ref", "--short", "HEAD"]);
    let status_before = git(fixture.repo(), &["status", "--porcelain=v1"]);
    let cached_before = git(fixture.repo(), &["diff", "--cached", "--binary"]);
    let stash_before = git(fixture.repo(), &["stash", "list"]);

    let outcome = invoke(
        &launcher,
        &[
            "repo",
            "attach",
            fixture.repo_str(),
            "--baseline=snapshot",
            "--confirm-snapshot",
            "--json",
        ],
    );

    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert_eq!(git(fixture.repo(), &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(fixture.repo(), &["symbolic-ref", "--short", "HEAD"]),
        branch_before
    );
    assert_eq!(
        git(fixture.repo(), &["status", "--porcelain=v1"]),
        status_before
    );
    assert_eq!(
        git(fixture.repo(), &["diff", "--cached", "--binary"]),
        cached_before
    );
    assert_eq!(git(fixture.repo(), &["stash", "list"]), stash_before);
    let snapshot_ref = json(&outcome.stdout)["attachment"]["attachment"]["snapshotRef"]
        .as_str()
        .expect("snapshot ref")
        .to_owned();
    assert!(!git(fixture.repo(), &["rev-parse", &snapshot_ref]).is_empty());
}

#[test]
fn suspicious_secret_paths_block_snapshot_and_are_never_persisted() {
    let fixture = Fixture::new();
    fixture.init_git();
    fixture.write("repo/README.md", "safe\n");
    fixture.commit();
    fixture.write("repo/.env", "OPENAI_API_KEY=do-not-store\n");
    let launcher = fixture.launcher();

    let outcome = invoke(
        &launcher,
        &[
            "repo",
            "attach",
            fixture.repo_str(),
            "--baseline",
            "snapshot",
            "--confirm-snapshot",
        ],
    );

    assert_eq!(outcome.code, 5);
    assert!(
        outcome
            .stderr
            .contains("repository.snapshot-secret-blocked")
    );
    assert!(!outcome.stderr.contains("do-not-store"));
    let state_text = read_tree_text(fixture.state());
    assert!(!state_text.contains("do-not-store"));
    assert!(!state_text.contains("OPENAI_API_KEY="));
}

#[test]
fn doctor_separates_categories_and_reports_exact_file_inventory_capabilities() {
    let fixture = Fixture::new();
    fixture.init_git();
    fixture.write(
        "repo/Cargo.toml",
        "[package]\nname='fixture'\nversion='0.0.0'\n",
    );
    fixture.write("repo/src/lib.rs", "pub fn fixture() {}\n");
    fixture.commit();
    let launcher = fixture
        .launcher()
        .with_provider_variables(["OPENAI_API_KEY".to_owned()]);
    let attached = invoke(
        &launcher,
        &["repo", "attach", fixture.repo_str(), "--baseline", "head"],
    );
    assert_eq!(attached.code, 0, "{}", attached.stderr);

    let report = launcher
        .doctor(&winwincode_cli::DoctorRequest {
            repository_path: fixture.repo().to_path_buf(),
        })
        .expect("doctor should produce a report");
    for category in ["product", "repository", "environment"] {
        assert!(report.checks.iter().any(|check| {
            serde_json::to_value(check.category)
                .expect("category json")
                .as_str()
                == Some(category)
        }));
    }
    let index = report.local_code_index.expect("index status");
    assert!(index.fresh);
    assert_eq!(index.mode, LocalCodeIndexMode::GitFileInventory);
    assert!(
        index
            .capabilities
            .supports(IndexCapability::ContentFingerprints)
    );
    assert!(!index.capabilities.supports(IndexCapability::SymbolOutlines));
    assert!(!index.capabilities.supports(IndexCapability::Callers));

    let human = invoke(&launcher, &["doctor", fixture.repo_str()]);
    assert!(human.stdout.contains("产品检查"));
    assert!(human.stdout.contains("仓库检查"));
    assert!(human.stdout.contains("环境检查"));
    assert!(human.stdout.contains("mode=git-file-inventory"));
    assert!(human.stdout.contains("fresh=true"));
    assert!(human.stdout.contains("content-fingerprints"));
    assert!(!human.stdout.contains("OPENAI_API_KEY="));
}

fn invoke(launcher: &SystemLocalLauncher, arguments: &[&str]) -> winwincode_cli::WwcCliExit {
    run_cli(
        &arguments
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>(),
        launcher,
    )
}

fn assert_snapshot_doctor(
    launcher: &SystemLocalLauncher,
    repository: &Path,
    ready: &serde_json::Value,
) {
    let report = launcher
        .doctor(&winwincode_cli::DoctorRequest {
            repository_path: repository.to_path_buf(),
        })
        .expect("doctor should use the attached snapshot");
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.code == "repository.snapshot-baseline-valid")
    );
    assert!(
        !report
            .checks
            .iter()
            .any(|check| check.code == "repository.head-missing")
    );
    assert_eq!(
        report
            .local_code_index
            .expect("snapshot index")
            .baseline_sha,
        ready["attachment"]["attachment"]["baselineSha"]
    );
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).expect("CLI JSON should parse")
}

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "winwincode-cli-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        let repo = root.join("repo");
        let state = root.join("state");
        fs::create_dir_all(&repo).expect("fixture repo directory");
        fs::create_dir_all(&state).expect("fixture state directory");
        Self { root, repo, state }
    }

    fn repo(&self) -> &Path {
        &self.repo
    }

    fn state(&self) -> &Path {
        &self.state
    }

    fn repo_str(&self) -> &str {
        self.repo().to_str().expect("UTF-8 fixture path")
    }

    fn launcher(&self) -> SystemLocalLauncher {
        SystemLocalLauncher::new(self.state())
    }

    fn write(&self, relative_path: &str, content: &str) {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent directory");
        }
        fs::write(path, content).expect("fixture file write");
    }

    fn init_git(&self) {
        run_git(self.repo(), &["init", "--quiet"]);
        run_git(self.repo(), &["config", "user.name", "WinWinCode Test"]);
        run_git(
            self.repo(),
            &["config", "user.email", "test@winwincode.invalid"],
        );
    }

    fn commit(&self) -> String {
        run_git(self.repo(), &["add", "."]);
        run_git(
            self.repo(),
            &["commit", "--quiet", "-m", "fixture baseline"],
        );
        git(self.repo(), &["rev-parse", "HEAD"])
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run_git(root: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .expect("git command should start");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(root: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .expect("git command should start");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_owned()
}

fn read_tree_text(root: &Path) -> String {
    if !root.exists() {
        return String::new();
    }
    let mut output = String::new();
    for entry in fs::read_dir(root).expect("read fixture state") {
        let path = entry.expect("state entry").path();
        if path.is_dir() {
            output.push_str(&read_tree_text(&path));
        } else if let Ok(text) = fs::read_to_string(path) {
            output.push_str(&text);
        }
    }
    output
}
