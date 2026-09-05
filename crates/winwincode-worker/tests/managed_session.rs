// SPDX-License-Identifier: Apache-2.0

//! Process-level checks for the `--managed-session` entry: argument
//! handling, config gating, and parity isolation against the `--remote`
//! entry. The parse and credential matrices live in
//! `src/managed_session.rs` unit tests; here the binary boundary proves
//! which identity source each entry consults.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("wwc-managed-cli-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("test root creates");
    root
}

fn write_file(path: &Path, contents: &str, mode: u32) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("test parent creates");
    }
    fs::write(path, contents).expect("test file writes");
    let mut permissions = fs::metadata(path).expect("test metadata").permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions).expect("test chmod");
}

fn valid_config_json() -> String {
    serde_json::json!({
        "clientNodeId": "cln_01JCLI",
        "clientInstanceId": "cli_01JCLI",
        "occupancyLeaseId": "ocq_01JCLI",
        "occupancyFencingToken": "42",
        "repositoryBindingId": "rbn_01JCLI",
        "workerSessionId": "wss_01JCLI",
        "workerId": "wrk_01JCLI",
        "workerInstanceId": "wri_01JCLI",
        "sourceDirectory": "/repo/winwincode",
        "dataDirectory": "/data/wrk_01JCLI",
        "serverOrigin": "https://127.0.0.1:1",
        "workerCredentialPath": "credential"
    })
    .to_string()
}

/// Runs the Worker binary with a deliberately empty environment so any
/// success-or-failure decision is attributable to the entry's declared
/// inputs only.
fn run_entry(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_winwincode-worker"))
        .args(args)
        .env_clear()
        .env("PATH", "")
        .output()
        .expect("worker binary runs")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn check_reports_the_managed_session_entry() {
    let output = run_entry(&["--check"]);
    assert!(output.status.success());
    let identity: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("identity is JSON");
    assert_eq!(identity["role"], "execution-worker");
    assert_eq!(identity["executionKernel"], "embedded-codex-core");
    assert_eq!(identity["externalFallback"], false);
    assert_eq!(identity["managedSession"], true);
}

#[test]
fn managed_session_requires_exactly_one_config_argument() {
    let missing = run_entry(&["--managed-session"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(stderr(&missing).contains("usage:"), "{}", stderr(&missing));

    let extra = run_entry(&["--managed-session", "a.json", "b.json"]);
    assert_eq!(extra.status.code(), Some(2));
    assert!(stderr(&extra).contains("usage:"), "{}", stderr(&extra));
}

#[test]
fn managed_session_refuses_a_missing_config_file() {
    let root = temp_root("missing");
    let output = run_entry(&[
        "--managed-session",
        root.join("absent.json").to_str().expect("utf-8 path"),
    ]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.contains("managed session config"), "{message}");
    assert!(message.contains("unavailable"), "{message}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn managed_session_refuses_a_non_0600_config_file() {
    let root = temp_root("permissions");
    let path = root.join("session.json");
    write_file(&path, &valid_config_json(), 0o644);
    let output = run_entry(&["--managed-session", path.to_str().expect("utf-8 path")]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.contains("0600"), "{message}");
    assert!(message.contains("644"), "{message}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn managed_session_names_missing_config_fields() {
    let root = temp_root("fields");
    let mut value: serde_json::Value =
        serde_json::from_str(&valid_config_json()).expect("valid config parses");
    value
        .as_object_mut()
        .expect("config is an object")
        .remove("workerId")
        .expect("workerId was present");
    let path = root.join("session.json");
    write_file(&path, &value.to_string(), 0o600);
    let output = run_entry(&["--managed-session", path.to_str().expect("utf-8 path")]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.contains("workerId"), "{message}");
    assert!(message.contains("missing"), "{message}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn managed_session_never_consults_remote_identity_environment() {
    // A fully valid config plus a valid private credential: the managed
    // entry must proceed past identity entirely and fail only on the
    // shared operational environment — even with hostile `--remote`
    // identity variables present. This pins that the two entries cannot
    // interfere with each other's identity source.
    let root = temp_root("isolation");
    let config_path = root.join("session.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&valid_config_json()).expect("valid config parses");
    value.as_object_mut().expect("config is an object").insert(
        "workerCredentialPath".to_owned(),
        serde_json::Value::String(
            root.join("credential")
                .to_str()
                .expect("utf-8 path")
                .to_owned(),
        ),
    );
    write_file(&config_path, &value.to_string(), 0o600);
    write_file(&root.join("credential"), "wsc-managed-token", 0o600);

    let output = Command::new(env!("CARGO_BIN_EXE_winwincode-worker"))
        .arg("--managed-session")
        .arg(config_path.to_str().expect("utf-8 path"))
        .env_clear()
        .env("PATH", "")
        // Poison every `--remote` identity variable: managed must ignore
        // them all.
        .env("WWC_WORKER_ID", "wrk_POISON")
        .env("WWC_WORKER_INSTANCE_ID", "wri_POISON")
        .env("WWC_WORKER_DATA_DIRECTORY", "/poison/data")
        .env("WWC_WORKER_SOURCE_ROOT", "/poison/source")
        .env("WWC_WORKER_SERVER_ORIGIN", "https://poison.invalid:1")
        .env("WWC_WORKER_CREDENTIAL_FILE", "/poison/credential")
        .output()
        .expect("worker binary runs");
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("WWC_WORKER_TLS_ROOT_DER_FILE"),
        "managed entry should reach the shared operational environment: {message}"
    );
    assert!(!message.contains("POISON"), "{message}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn remote_entry_still_requires_its_own_identity_environment() {
    // Parity guard: `--remote` behaviour is unchanged — with no environment
    // it fails on its first identity variable, exactly as before this lane.
    let output = run_entry(&["--remote"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.contains("WWC_WORKER_ID"), "{message}");
}
