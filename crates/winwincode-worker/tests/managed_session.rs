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
                    failure: None,
                    status: 403,
                    body: String::new(),
                }
            }
            WorkerRelayRequest::Accounting { body: None, .. } => WorkerRelayResponse {
                failure: None,
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
fn permanent_registration_failures_exit_without_waiting_for_device_pipe_closure() {
    use std::io::Read;
    use std::time::{Duration, Instant};
    use winwincode_network::{Acceptance, ErrorKind, NetworkFailure, Phase};
    for failure in [
        NetworkFailure::http(401, None),
        NetworkFailure::http(403, None),
        NetworkFailure::http(404, None),
        NetworkFailure::http(413, None),
        NetworkFailure::new(
            ErrorKind::ProtocolInvalid,
            Acceptance::ResponseReceived,
            Phase::Decode,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("source")).unwrap();
        let config_path = root.path().join("session.json");
        fs::write(&config_path, valid_config(root.path()).to_string()).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_winwincode-worker"))
            .args(["--managed-session", config_path.to_str().unwrap()])
            .env_clear()
            .env("PATH", "")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        assert_ne!(output.read_line(&mut line).unwrap(), 0);
        assert!(matches!(
            serde_json::from_str::<WorkerRelayRequest>(&line).unwrap(),
            WorkerRelayRequest::Exchange { .. }
        ));
        serde_json::to_writer(
            &mut input,
            &WorkerRelayResponse {
                status: failure.http_status.unwrap_or(502),
                body: String::new(),
                failure: Some(failure),
            },
        )
        .unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(!status.success());
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("permanent registration error {failure} did not stop Worker");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut remaining = String::new();
        output.read_to_string(&mut remaining).unwrap();
        assert!(
            remaining.is_empty(),
            "permanent failure replayed registration"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn shutdown_recovery_closes_pending_calls_and_preserves_unknown_usage_and_retained_results() {
    use sha2::{Digest, Sha256};
    use std::time::{Duration, Instant};
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("source")).unwrap();
    let config = valid_config(root.path());
    let config_path = root.path().join("session.json");
    fs::write(&config_path, config.to_string()).unwrap();
    drop(winwincode_provider::DeviceProviderStore::open(&root.path().join("providers")).unwrap());
    let db = rusqlite::Connection::open(root.path().join("providers/providers.sqlite3")).unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let mut open = fixture["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["kind"] == "model.open")
        .unwrap()
        .clone();
    open["lease"]["workerInstanceId"] = config["workerInstanceId"].clone();
    let exchange_id = open["modelExchangeId"].as_str().unwrap().to_owned();
    let request = open.to_string();
    db.execute(
        "INSERT INTO exchanges(exchange_id,digest,request_open) VALUES(?1,?2,?3)",
        rusqlite::params![
            exchange_id,
            format!("{:x}", Sha256::digest(request.as_bytes())),
            request
        ],
    )
    .unwrap();
    let payload = b"synthetic original physical Provider request";
    db.execute("INSERT INTO model_invocation_attempts(exchange_id,attempt_number,adapter_request_id,state,request_json,request_payload) VALUES(?1,1,'synthetic-old-attempt','invoking',?2,?3)", rusqlite::params![exchange_id, request, payload.as_slice()]).unwrap();
    open["modelExchangeId"] = "mex_00000000000000000000000099".into();
    let retained_open = serde_json::from_value(open.clone()).unwrap();
    let retained_chunks = vec![winwincode_provider::model_failure(
        &retained_open,
        "SYNTHETIC_RETAINED_RESPONSE",
    )];
    winwincode_provider::DeviceProviderStore::open(&root.path().join("providers"))
        .unwrap()
        .retain_stored_model_exchange_chunks(&retained_open, &retained_chunks)
        .unwrap();
    let retained_request = serde_json::to_string(&retained_open).unwrap();
    let retained_result = serde_json::to_string(&retained_chunks).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_winwincode-worker"))
        .args(["--managed-session", config_path.to_str().unwrap()])
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
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Worker shutdown did not finish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    for _ in 0..2 {
        let output = run_entry(&["--recover-interrupted-calls", config_path.to_str().unwrap()]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let (saved, state, failure, accounting): (Vec<u8>, String, String, Option<String>) = db.query_row("SELECT request_payload,state,failure_chunks,accounting_chunks FROM model_invocation_attempts", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
    assert_eq!(saved, payload);
    assert_eq!(state, "interrupted_unknown");
    assert!(failure.contains("DEVICE_MODEL_INTERRUPTED"));
    assert!(accounting.is_none());
    let count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM model_invocation_attempts",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let retained: (String, String) = db
        .query_row(
            "SELECT request_open,chunks FROM exchanges WHERE exchange_id=?1",
            [open["modelExchangeId"].as_str().unwrap()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(retained, (retained_request, retained_result));
    let (saved_request, status): (String, String) = db
        .query_row(
            "SELECT request_open,call_status FROM exchanges WHERE exchange_id=?1",
            [exchange_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(saved_request, request);
    assert_eq!(status, "failed");
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
