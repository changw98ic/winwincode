// SPDX-License-Identifier: Apache-2.0

//! Coverage for the `wwc repo add|list|remove` Device Client registry
//! surface (plan §13.1, §16.8): the registration check chain drives the
//! canonical device-client library, a non-Git directory needs the explicit
//! `--init` confirmation, list shows the local bindings, remove reports the
//! removal, and every server-bound frame stays path-free.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use sha2::Digest as _;
use winwincode_cli::{
    RepoAdminError, RepoAdminOutcome, SystemLocalLauncher, WwcCliExit, repo_add, repo_list,
    repo_remove, run_cli,
};
use winwincode_device_client::{
    DeviceStore, IssuedEnrollment, adopt_enrollment, ensure_device_identity, load_device_identity,
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

const ASSIGNED_NODE: &str = "cnd_D1D1D1D1D1D1D1D1D1D1D1D1D1";
const ASSIGNED_PUBLIC_CLIENT_ID: &str = "1928374650";
const ISSUED_SECRET: [u8; 32] = [0x6d; 32];
const STAMP: &str = "2026-09-04T00:00:00.000Z";

struct Fixture {
    data_directory: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let data_directory = std::env::temp_dir().join(format!(
            "winwincode-cli-repo-tests-{}-{sequence}",
            std::process::id()
        ));
        let workspace = std::env::temp_dir().join(format!(
            "winwincode-cli-repo-workspaces-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&data_directory);
        let _ = fs::remove_dir_all(&workspace);
        fs::create_dir_all(&workspace).expect("workspace directory");
        Self {
            data_directory,
            workspace,
        }
    }

    fn data_dir(&self) -> &str {
        self.data_directory.to_str().expect("UTF-8 fixture path")
    }

    fn cli(&self, arguments: &[&str]) -> WwcCliExit {
        let mut full = vec!["repo"];
        full.extend(arguments.iter().copied());
        full.extend(["--data-dir", self.data_dir()]);
        run_cli(
            &full
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
            &SystemLocalLauncher::new(std::env::temp_dir()),
        )
    }

    /// Creates the device identity and adopts the enrollment, the state the
    /// Device Client reaches after its first accepted exchange.
    fn enroll(&self) {
        let mut store = DeviceStore::open(&self.data_directory).expect("device store opens");
        ensure_device_identity(&mut store, &seed(), STAMP).expect("identity loads");
        let device_id = load_device_identity(&store)
            .expect("identity read")
            .expect("fresh identity")
            .identity()
            .device_id()
            .to_owned();
        let mut secret_hex = String::with_capacity(ISSUED_SECRET.len() * 2);
        for byte in ISSUED_SECRET {
            use std::fmt::Write as _;
            let _ = write!(secret_hex, "{byte:02x}");
        }
        adopt_enrollment(
            &mut store,
            &device_id,
            &IssuedEnrollment {
                client_node_id: ASSIGNED_NODE.to_owned(),
                public_client_id: ASSIGNED_PUBLIC_CLIENT_ID.to_owned(),
                credential_material: secret_hex,
                credential_digest: format!("sha256:{:x}", sha2::Sha256::digest(ISSUED_SECRET)),
            },
            STAMP,
        )
        .expect("enrollment adopts");
        store.close().expect("store closes");
    }

    /// Creates a real Git repository with one baseline commit.
    fn git_repository(&self, name: &str) -> PathBuf {
        let root = self.workspace.join(name);
        fs::create_dir_all(&root).expect("repository directory");
        git(&root, &["init"]);
        git(&root, &["config", "user.email", "registry@example.test"]);
        git(&root, &["config", "user.name", "Registry Tests"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        git(&root, &["commit", "--allow-empty", "-m", "baseline"]);
        root
    }

    fn pending_kinds(&self) -> Vec<String> {
        let store = DeviceStore::open(&self.data_directory).expect("device store opens");
        let kinds = store
            .pending_outbox_envelopes()
            .expect("pending frames read")
            .into_iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>();
        store.close().expect("store closes");
        kinds
    }

    fn cleanup(&self) {
        let _ = fs::remove_dir_all(&self.data_directory);
        let _ = fs::remove_dir_all(&self.workspace);
    }
}

fn seed() -> winwincode_device_client::DeviceIdentitySeed {
    winwincode_device_client::DeviceIdentitySeed {
        display_name: "wwc repo tests".to_owned(),
        platform: "cli".to_owned(),
        architecture: "cli".to_owned(),
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// Runs one git command with an isolated configuration and fails on error.
fn git(root: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(arguments)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git should run");
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn require_git() {
    let available = Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    assert!(available, "system git must be available on PATH");
}

#[test]
fn repo_add_list_remove_round_trip_reports_frames() {
    require_git();
    let fixture = Fixture::new();
    fixture.enroll();
    let repository = fixture.git_repository("round-trip-repo");

    // add: JSON outcome carries the binding view.
    let repository_text = repository.to_str().expect("UTF-8 repository path");
    let exit = fixture.cli(&["add", repository_text, "--json"]);
    assert_eq!(exit.code, 0, "add failed: {} {}", exit.stdout, exit.stderr);
    let added: Value = serde_json::from_str(&exit.stdout).expect("add JSON");
    assert_eq!(added["status"], "registered");
    let binding_id = added["repository"]["repositoryBindingId"]
        .as_str()
        .expect("binding id")
        .to_owned();
    assert!(binding_id.starts_with("rbd_"));
    let default_branch = added["repository"]["defaultBranch"]
        .as_str()
        .expect("default branch")
        .to_owned();
    assert!(!default_branch.is_empty());
    assert_eq!(
        added["repository"]["canonicalPath"].as_str(),
        Some(
            fs::canonicalize(&repository)
                .expect("canonicalize")
                .to_str()
                .expect("UTF-8")
        )
    );
    assert_eq!(added["gitInitialized"], false);

    // The upsert frame is durable and path-free.
    let kinds = fixture.pending_kinds();
    assert_eq!(kinds, vec!["client.repository.upsert".to_owned()]);
    {
        let store = DeviceStore::open(&fixture.data_directory).expect("store opens");
        let entries = store.pending_outbox_envelopes().expect("pending frames");
        let encoded = String::from_utf8_lossy(&entries[0].payload).into_owned();
        assert!(!encoded.contains(repository_text), "frame carries a path");
        store.close().expect("store closes");
    }

    // list: the binding appears with its availability.
    let exit = fixture.cli(&["list", "--json"]);
    assert_eq!(exit.code, 0, "list failed: {} {}", exit.stdout, exit.stderr);
    let listed: Value = serde_json::from_str(&exit.stdout).expect("list JSON");
    assert_eq!(listed["status"], "list");
    let repositories = listed["repositories"].as_array().expect("repositories");
    assert_eq!(repositories.len(), 1);
    assert_eq!(
        repositories[0]["repositoryBindingId"].as_str(),
        Some(binding_id.as_str())
    );
    assert_eq!(repositories[0]["availability"], "available");

    // remove: the removal is reported and the binding disappears.
    let exit = fixture.cli(&["remove", &binding_id, "--json"]);
    assert_eq!(
        exit.code, 0,
        "remove failed: {} {}",
        exit.stdout, exit.stderr
    );
    let removed: Value = serde_json::from_str(&exit.stdout).expect("remove JSON");
    assert_eq!(removed["status"], "removed");
    assert_eq!(removed["repositoryBindingId"], binding_id.as_str());
    assert_eq!(
        fixture.pending_kinds(),
        vec![
            "client.repository.upsert".to_owned(),
            "client.repository.removed".to_owned(),
        ]
    );

    let exit = fixture.cli(&["list", "--json"]);
    let listed: Value = serde_json::from_str(&exit.stdout).expect("list JSON");
    assert_eq!(listed["repositories"].as_array().expect("empty").len(), 0);

    fixture.cleanup();
}

#[test]
fn repo_add_refuses_non_git_directory_until_confirmed() {
    require_git();
    let fixture = Fixture::new();
    fixture.enroll();
    let plain = fixture.workspace.join("plain-directory");
    fs::create_dir_all(&plain).expect("plain directory");
    let plain_text = plain.to_str().expect("UTF-8 path");

    let exit = fixture.cli(&["add", plain_text]);
    assert_eq!(exit.code, 3, "expected action-required: {exit:?}");
    assert!(
        exit.stderr.contains("invalid_git"),
        "stderr: {}",
        exit.stderr
    );

    // Nothing persisted, nothing reported.
    assert!(fixture.pending_kinds().is_empty());

    // With --init the directory initializes and registers.
    let exit = fixture.cli(&["add", plain_text, "--init"]);
    assert_eq!(
        exit.code, 0,
        "add --init failed: {} {}",
        exit.stdout, exit.stderr
    );
    assert!(plain.join(".git").is_dir(), "git init ran");
    assert_eq!(
        fixture.pending_kinds(),
        vec!["client.repository.upsert".to_owned()]
    );

    fixture.cleanup();
}

#[test]
fn repo_commands_require_enrollment_and_data_dir() {
    require_git();
    let fixture = Fixture::new();
    let repository = fixture.git_repository("unenrolled-repo");
    let repository_text = repository.to_str().expect("UTF-8 repository path");

    // A data directory without a device identity is not initialized.
    let exit = fixture.cli(&["add", repository_text]);
    assert_eq!(exit.code, 3, "expected action-required: {exit:?}");

    // After first boot but before enrollment, frames are refused.
    {
        let mut store = DeviceStore::open(&fixture.data_directory).expect("store opens");
        ensure_device_identity(&mut store, &seed(), STAMP).expect("identity loads");
        store.close().expect("store closes");
    }
    let exit = fixture.cli(&["add", repository_text]);
    assert_eq!(exit.code, 3, "expected action-required: {exit:?}");
    assert!(
        exit.stdout.contains("enrollment"),
        "guidance expected: {}",
        exit.stdout
    );

    // Missing --data-dir is a usage error.
    let exit = run_cli(
        &["repo".to_owned(), "list".to_owned()],
        &SystemLocalLauncher::new(std::env::temp_dir()),
    );
    assert_eq!(exit.code, 2, "usage error expected: {exit:?}");

    fixture.cleanup();
}

#[test]
fn repo_remove_unknown_binding_is_action_required() {
    let fixture = Fixture::new();
    fixture.enroll();
    let outcome = repo_remove(
        &fixture.data_directory,
        "rbd_UNKNOWNUNKNOWNUNKNOWNUNKNOWN00",
    );
    assert!(matches!(outcome, Err(RepoAdminError::NotFound)));
    fixture.cleanup();
}

#[test]
fn repo_list_outcome_renders_bindings() {
    require_git();
    let fixture = Fixture::new();
    fixture.enroll();
    let repository = fixture.git_repository("listed-repo");
    let outcome = repo_add(&fixture.data_directory, &repository, false).expect("registration");
    let RepoAdminOutcome::Registered {
        repository: view, ..
    } = outcome
    else {
        panic!("expected a registration");
    };

    let outcome = repo_list(&fixture.data_directory).expect("list");
    let RepoAdminOutcome::List { repositories } = outcome else {
        panic!("expected a list");
    };
    assert_eq!(repositories.len(), 1);
    // The list view is built from the durable rows: the default branch is
    // not persisted, everything else matches the registration view.
    let listed = &repositories[0];
    assert_eq!(listed.repository_binding_id, view.repository_binding_id);
    assert_eq!(listed.canonical_path, view.canonical_path);
    assert_eq!(listed.git_common_directory, view.git_common_directory);
    assert_eq!(listed.availability, view.availability);
    assert_eq!(listed.dirty_state, view.dirty_state);
    assert_eq!(listed.head_commit, view.head_commit);
    assert_eq!(listed.last_scanned_at, view.last_scanned_at);
    assert_eq!(listed.last_canonicalized_at, view.last_canonicalized_at);
    assert_eq!(listed.default_branch, None);

    fixture.cleanup();
}
