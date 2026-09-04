// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::process::{Pid, test_kill_process};
use tempfile::TempDir;
use tokio::time::sleep;
use winwincode_domain::WorkspaceRevision;
use winwincode_execution_port::generated::{ValidationProfileName, ValidationReceiptStatus};
use winwincode_execution_port::validation_config::{
    parse_validation_configuration, suggest_validation_profile,
};
use winwincode_worker::workspace_phase::{
    ConfiguredPhasePlan, PhaseAccess, PhaseCancellation, PhaseCommand, PhaseProcessErrorCode,
    PhaseProcessRunner, PhaseProcessStatus,
};

struct Fixture {
    root: TempDir,
    workspace: PathBuf,
    scratch: PathBuf,
}

#[test]
fn explicit_profiles_are_rebuilt_in_declared_order_and_markdown_inference_stays_changed() {
    let fixture = Fixture::new();
    let configuration = parse_validation_configuration(
        r#"schemaVersion = 1

[[commands]]
id = "python-format"
phase = "formatter"
language = "python"
allowedCompanionPaths = ["generated/manifest.json"]
argv = ["/usr/bin/python3", "-B", "-c", "print('format')"]
workingDirectory = "."
environment = []
network = false
timeoutMillis = 300000
outputLimitBytes = 1048576

[[commands]]
id = "rust-check"
phase = "validation"
language = "rust"
allowedCompanionPaths = []
argv = ["/usr/bin/true"]
workingDirectory = "."
environment = []
network = false
timeoutMillis = 300000
outputLimitBytes = 1048576

[[commands]]
id = "typescript-check"
phase = "validation"
language = "typescript"
allowedCompanionPaths = []
argv = ["/usr/bin/true"]
workingDirectory = "."
environment = []
network = false
timeoutMillis = 300000
outputLimitBytes = 1048576

[[profiles]]
name = "changed"
commandIds = ["python-format", "rust-check"]

[[profiles]]
name = "fast"
commandIds = ["rust-check"]

[[profiles]]
name = "affected"
commandIds = ["rust-check", "typescript-check"]

[[profiles]]
name = "final"
commandIds = ["typescript-check", "rust-check"]
"#,
    )
    .expect("canonical explicit config");
    let plan = ConfiguredPhasePlan::from_explicit_configuration(
        &configuration,
        "affected",
        &["src/lib.rs".to_owned()],
        &fixture.workspace,
        &fixture.scratch,
        OsString::from("/usr/bin:/bin").as_os_str(),
        None,
    )
    .expect("affected plan");
    assert_eq!(plan.profile(), &ValidationProfileName::Affected);
    assert!(plan.writer_commands.is_empty());
    assert!(plan.allowed_writer_paths.is_empty());
    assert_eq!(
        plan.validation_commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["rust-check", "typescript-check"]
    );
    let suggestion =
        suggest_validation_profile(&["README.md".to_owned()]).expect("Markdown-only suggestion");
    assert_eq!(suggestion.profile, ValidationProfileName::Changed);
    assert!(!suggestion.executable);
    assert!(suggestion.command_ids.is_empty());
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary root");
        let workspace = root.path().join("workspace");
        let scratch = workspace.join(".winwincode-scratch");
        fs::create_dir_all(&scratch).expect("scratch directory");
        Self {
            root,
            workspace,
            scratch,
        }
    }

    fn python(&self, script: &str, access: PhaseAccess) -> PhaseCommand {
        PhaseCommand {
            name: "python-check".to_owned(),
            diagnostic_parser_version: None,
            program: PathBuf::from("/usr/bin/python3"),
            arguments: vec![
                OsString::from("-B"),
                OsString::from("-c"),
                OsString::from(script),
            ],
            working_directory: self.workspace.clone(),
            scratch_directory: self.scratch.clone(),
            environment: BTreeMap::new(),
            access,
            timeout: Duration::from_secs(5),
            max_output_bytes: 1024,
        }
    }
}

#[tokio::test]
async fn argv_cwd_environment_network_and_read_only_workspace_are_fail_closed() {
    let fixture = Fixture::new();
    let runner = PhaseProcessRunner;
    let cancellation = PhaseCancellation::default();
    let mut command = fixture.python(
        "from pathlib import Path; Path('forbidden.txt').write_text('write')",
        PhaseAccess::ReadOnlyValidation,
    );
    let receipt = runner
        .execute(&fixture.workspace, &command, &cancellation)
        .await
        .expect("sandboxed validation command");
    assert_eq!(receipt.status, PhaseProcessStatus::Failed);
    assert!(!fixture.workspace.join("forbidden.txt").exists());

    command.environment.insert(
        OsString::from("TOKEN"),
        OsString::from("must-not-be-inherited"),
    );
    let error = runner
        .execute(&fixture.workspace, &command, &cancellation)
        .await
        .expect_err("secret-like environment entry must be rejected");
    assert_eq!(error.code, PhaseProcessErrorCode::InvalidCommand);

    command.environment.clear();
    command.working_directory = fixture.root.path().to_path_buf();
    let error = runner
        .execute(&fixture.workspace, &command, &cancellation)
        .await
        .expect_err("foreign cwd must be rejected");
    assert_eq!(error.code, PhaseProcessErrorCode::InvalidCommand);
}

#[tokio::test]
async fn writer_can_change_only_the_workspace_and_output_is_digest_bounded() {
    let fixture = Fixture::new();
    let runner = PhaseProcessRunner;
    let command = fixture.python(
        "from pathlib import Path; Path('formatted.py').write_text('ok\\n'); print('done')",
        PhaseAccess::Writer,
    );
    let execution = runner
        .execute_with_output(&fixture.workspace, &command, &PhaseCancellation::default())
        .await
        .expect("writer command");
    assert_eq!(execution.receipt.status, PhaseProcessStatus::Passed);
    assert_eq!(execution.stdout, b"done\n");
    assert_eq!(
        execution.receipt.output_bytes,
        execution.stdout.len() + execution.stderr.len()
    );
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("formatted.py")).expect("formatted file"),
        "ok\n"
    );
    assert!(execution.receipt.stdout_digest.0.starts_with("sha256:"));
    assert!(execution.receipt.stderr_digest.0.starts_with("sha256:"));

    let mut excessive = fixture.python("print('x' * 100000)", PhaseAccess::ReadOnlyValidation);
    excessive.max_output_bytes = 128;
    let execution = runner
        .execute_with_output(
            &fixture.workspace,
            &excessive,
            &PhaseCancellation::default(),
        )
        .await
        .expect("bounded output command");
    assert_eq!(
        execution.receipt.status,
        PhaseProcessStatus::OutputLimitExceeded
    );
    assert!(execution.stdout.len() <= excessive.max_output_bytes);
    assert!(execution.stderr.len() <= excessive.max_output_bytes);
}

#[tokio::test]
async fn validation_receipt_continues_after_failure_and_binds_the_exact_read_only_tree() {
    let fixture = Fixture::new();
    let commands = [
        fixture.python("print('parser passed')", PhaseAccess::ReadOnlyValidation),
        fixture.python(
            "from pathlib import Path; Path('forbidden.py').write_text('bad')",
            PhaseAccess::ReadOnlyValidation,
        ),
        fixture.python("raise SystemExit(0)", PhaseAccess::ReadOnlyValidation),
    ];
    let revision = WorkspaceRevision(format!("git-tree:{}", "a".repeat(40)));
    let receipt = PhaseProcessRunner
        .validate(
            &fixture.workspace,
            &commands,
            &ValidationProfileName::Changed,
            &revision,
            &PhaseCancellation::default(),
        )
        .await
        .expect("validation receipt");
    assert_eq!(receipt.status, ValidationReceiptStatus::Failed);
    assert_eq!(receipt.base_revision, revision);
    assert_eq!(receipt.result_revision.as_ref(), Some(&revision));
    assert_eq!(receipt.checks.len(), 3);
    assert!(!fixture.workspace.join("forbidden.py").exists());
}

#[tokio::test]
async fn rust_validation_writes_only_to_the_runtime_owned_target_directory() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.workspace.join("src")).expect("Rust source directory");
    fs::write(
        fixture.workspace.join("Cargo.toml"),
        "[package]\nname='phase-fixture'\nversion='0.1.0'\nedition='2024'\n",
    )
    .expect("Rust manifest");
    fs::write(
        fixture.workspace.join("src/lib.rs"),
        "pub fn value() -> u8 { 1 }\n",
    )
    .expect("Rust source");
    let trusted_path = std::env::var_os("PATH").expect("trusted PATH");
    let cargo = std::env::split_paths(&trusted_path)
        .map(|directory| directory.join("cargo"))
        .find(|candidate| candidate.is_file())
        .expect("cargo executable");
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")))
        .expect("rustup home")
        .canonicalize()
        .expect("canonical rustup home");
    assert!(
        std::process::Command::new(&cargo)
            .args(["generate-lockfile", "--offline"])
            .current_dir(&fixture.workspace)
            .status()
            .expect("generate fixture lock")
            .success()
    );
    let mut environment = BTreeMap::new();
    environment.insert(OsString::from("PATH"), trusted_path);
    environment.insert(
        OsString::from("HOME"),
        fixture.scratch.as_os_str().to_os_string(),
    );
    environment.insert(
        OsString::from("TMPDIR"),
        fixture.scratch.as_os_str().to_os_string(),
    );
    environment.insert(
        OsString::from("CARGO_HOME"),
        fixture.scratch.join("cargo-home").into_os_string(),
    );
    environment.insert(
        OsString::from("CARGO_TARGET_DIR"),
        fixture.scratch.join("cargo-target").into_os_string(),
    );
    environment.insert(OsString::from("RUSTUP_HOME"), rustup_home.into_os_string());
    let command = PhaseCommand {
        name: "rust-check".to_owned(),
        diagnostic_parser_version: None,
        program: cargo,
        arguments: ["check", "--locked", "--offline"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        working_directory: fixture.workspace.clone(),
        scratch_directory: fixture.scratch.clone(),
        environment,
        access: PhaseAccess::ReadOnlyValidation,
        timeout: Duration::from_secs(30),
        max_output_bytes: 1_048_576,
    };
    let receipt = PhaseProcessRunner
        .execute(&fixture.workspace, &command, &PhaseCancellation::default())
        .await
        .expect("sandboxed cargo check");
    assert_eq!(receipt.status, PhaseProcessStatus::Passed);
    assert!(!fixture.workspace.join("target").exists());
    assert!(fixture.scratch.join("cargo-target").is_dir());
}

#[tokio::test]
async fn typescript_validation_runs_in_the_read_only_workspace_without_changing_it() {
    let fixture = Fixture::new();
    let source = b"const answer: number = 42;\nif (answer !== 42) throw new Error('bad');\n";
    let source_path = fixture.workspace.join("fixture.ts");
    fs::write(&source_path, source).expect("TypeScript fixture");
    let node = std::env::split_paths(&std::env::var_os("PATH").expect("trusted PATH"))
        .map(|directory| directory.join("node"))
        .find(|candidate| candidate.is_file())
        .expect("Node executable");
    let command = PhaseCommand {
        name: "typescript-check".to_owned(),
        diagnostic_parser_version: None,
        program: node,
        arguments: vec![OsString::from("fixture.ts")],
        working_directory: fixture.workspace.clone(),
        scratch_directory: fixture.scratch.clone(),
        environment: BTreeMap::new(),
        access: PhaseAccess::ReadOnlyValidation,
        timeout: Duration::from_secs(10),
        max_output_bytes: 1_048_576,
    };
    let receipt = PhaseProcessRunner
        .execute(&fixture.workspace, &command, &PhaseCancellation::default())
        .await
        .expect("sandboxed TypeScript validation");
    assert_eq!(receipt.status, PhaseProcessStatus::Passed);
    assert_eq!(
        fs::read(&source_path).expect("unchanged TypeScript"),
        source
    );
    let mut entries = fs::read_dir(&fixture.workspace)
        .expect("workspace entries")
        .map(|entry| entry.expect("workspace entry").file_name())
        .collect::<Vec<_>>();
    entries.sort();
    assert_eq!(
        entries,
        [
            OsString::from(".winwincode-scratch"),
            OsString::from("fixture.ts")
        ]
    );
}

#[tokio::test]
async fn timeout_and_cancel_reap_the_complete_process_group() {
    let fixture = Fixture::new();
    let child_pid = fixture.scratch.join("child.pid");
    let script = format!(
        "import pathlib, subprocess, time; p=subprocess.Popen(['/bin/sleep','30']); \
         pathlib.Path({child_pid:?}).write_text(str(p.pid)); time.sleep(30)"
    );
    let mut timed = fixture.python(&script, PhaseAccess::ReadOnlyValidation);
    timed.timeout = Duration::from_secs(1);
    let receipt = PhaseProcessRunner
        .execute(&fixture.workspace, &timed, &PhaseCancellation::default())
        .await
        .expect("timed command");
    assert_eq!(receipt.status, PhaseProcessStatus::TimedOut);
    assert_process_gone(read_pid(&child_pid)).await;

    let cancelled_pid = fixture.scratch.join("cancelled-child.pid");
    let script = format!(
        "import pathlib, subprocess, time; p=subprocess.Popen(['/bin/sleep','30']); \
         pathlib.Path({cancelled_pid:?}).write_text(str(p.pid)); time.sleep(30)"
    );
    let command = fixture.python(&script, PhaseAccess::ReadOnlyValidation);
    let cancellation = PhaseCancellation::default();
    let trigger = cancellation.clone();
    tokio::spawn(async move {
        sleep(Duration::from_secs(1)).await;
        trigger.cancel();
    });
    let receipt = PhaseProcessRunner
        .execute(&fixture.workspace, &command, &cancellation)
        .await
        .expect("cancelled command");
    assert_eq!(receipt.status, PhaseProcessStatus::Cancelled);
    assert_process_gone(read_pid(&cancelled_pid)).await;
}

fn read_pid(path: &Path) -> Pid {
    let raw = fs::read_to_string(path)
        .expect("child pid")
        .parse::<i32>()
        .expect("numeric child pid");
    Pid::from_raw(raw).expect("positive child pid")
}

async fn assert_process_gone(pid: Pid) {
    for _ in 0..100 {
        if test_kill_process(pid) == Err(rustix::io::Errno::SRCH) {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("process group descendant remained alive");
}
