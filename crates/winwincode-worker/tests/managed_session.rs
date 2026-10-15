// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use winwincode_client_port::worker_relay::{WorkerRelayRequest, WorkerRelayResponse};
use winwincode_execution_port::transport::{RemoteExchangeRequest, RemoteTransportAdapter};

fn valid_config(root: &std::path::Path) -> serde_json::Value {
    serde_json::json!({
        "clientNodeId": "cln_00000000000000000000000001",
        "clientInstanceId": "cli_00000000000000000000000001",
        "occupancyLeaseId": "ocq_00000000000000000000000001",
        "occupancyFencingToken": "42",
        "repositoryBindingId": "rbn_00000000000000000000000001",
        "workerSessionId": "wss_00000000000000000000000001",
        "workerId": "wrk_00000000000000000000000001",
        "workerInstanceId": "wki_00000000000000000000000001",
        "sourceDirectory": root.join("source"),
        "dataDirectory": root.join("worker"),
        "providerDirectory": root.join("providers"),
        "helperExecutable": root.join("winwincode-kernel-helper")
    })
}

fn run_entry(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_winwincode-worker"))
        .args(args)
        .env_clear()
        .env("PATH", "")
        .output()
        .expect("worker binary runs")
}

#[test]
fn check_reports_the_managed_session_entry() {
    let output = run_entry(&["--check"]);
    assert!(output.status.success());
    let identity: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(identity["role"], "execution-worker");
    assert_eq!(identity["executionKernel"], "embedded-codex-core");
    assert_eq!(identity["externalFallback"], false);
    assert_eq!(identity["managedSession"], true);
}

#[test]
fn managed_session_requires_one_configuration_argument() {
    for args in [
        vec!["--managed-session"],
        vec!["--managed-session", "a.json", "b.json"],
        vec!["--remote"],
    ] {
        let output = run_entry(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("usage:"));
    }
}

#[test]
fn managed_session_reports_read_and_parse_errors() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.json");
    let missing = run_entry(&["--managed-session", path.to_str().unwrap()]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("cannot read"));
    let mut config = valid_config(root.path());
    config.as_object_mut().unwrap().remove("workerId");
    fs::write(&path, config.to_string()).unwrap();
    let invalid = run_entry(&["--managed-session", path.to_str().unwrap()]);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("workerId"));
}

#[test]
fn managed_session_registers_over_device_stdio_without_provider_or_helper_preflight() {
    struct Core;
    impl winwincode_execution_port::transport::ExecutionPortCore for Core {
        type Output = ();
        type Error = std::convert::Infallible;
        fn accept(
            &mut self,
            _: &winwincode_execution_port::generated::ExecutionPortMessage,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("source")).unwrap();
    fs::write(root.path().join("providers"), "unavailable provider store").unwrap();
    let config_path = root.path().join("session.json");
    let config = valid_config(root.path());
    fs::write(&config_path, config.to_string()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_winwincode-worker"))
        .arg("--managed-session")
        .arg(&config_path)
        .env_clear()
        .env("PATH", "")
        .env("WWC_WORKER_ID", "POISON")
        .env("WWC_WORKER_SERVER_ORIGIN", "https://poison.invalid:1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut registration = None;
    for _ in 0..3 {
        let mut line = String::new();
        if output.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let request: WorkerRelayRequest = serde_json::from_str(&line).unwrap();
        let response = match request {
            WorkerRelayRequest::Exchange { body } => {
                let exchange = RemoteExchangeRequest::decode(body.as_bytes()).unwrap();
                let frame = RemoteTransportAdapter::<Core>::decode(exchange.frame()).unwrap();
                registration = Some(frame.message().clone());
                WorkerRelayResponse {
                    status: 403,
                    body: String::new(),
                }
            }
            WorkerRelayRequest::Accounting { body: None, .. } => WorkerRelayResponse {
                status: 200,
                body: serde_json::json!({"leases": [], "nextOffset": null}).to_string(),
            },
            WorkerRelayRequest::Accounting { body: Some(_), .. } => {
                panic!("registration has no accounting statement")
            }
        };
        serde_json::to_writer(&mut input, &response).unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
        if registration.is_some() {
            break;
        }
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    let message = registration.unwrap_or_else(|| {
        panic!(
            "registration missing: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let winwincode_execution_port::generated::ExecutionPortMessage::WorkerRegisterMessage(register) =
        message
    else {
        panic!("first exchange is not registration")
    };
    assert_eq!(register.worker_id.0, config["workerId"].as_str().unwrap());
    assert_eq!(
        register.worker_instance_id.0,
        config["workerInstanceId"].as_str().unwrap()
    );
    assert!(root.path().join("providers").is_file());
    assert!(!root.path().join("winwincode-kernel-helper").exists());
}

#[test]
fn managed_session_terminates_while_device_keeps_the_response_pipe_open() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("source")).unwrap();
    let config_path = root.path().join("session.json");
    fs::write(&config_path, valid_config(root.path()).to_string()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_winwincode-worker"))
        .arg("--managed-session")
        .arg(&config_path)
        .env_clear()
        .env("PATH", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _response_pipe = child.stdin.take().unwrap();
    let mut requests = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    assert_ne!(requests.read_line(&mut line).unwrap(), 0);
    serde_json::from_str::<WorkerRelayRequest>(&line).unwrap();
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Worker termination waited for the unresponsive Device pipe");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
