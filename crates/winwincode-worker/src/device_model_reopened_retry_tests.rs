// SPDX-License-Identifier: Apache-2.0

//! Same live Worker reopens the original durable retry under three real HTTPS slots.

use super::*;
use crate::device_model::IntakeLogLine;

const TEST: &str = "device_model::concurrency_tests::physical_attempt_tests::reopened_retry_tests::same_live_worker_reopens_durable_retries_under_three_real_slots";
const AUTHORITY: usize = 607;

#[test]
fn same_live_worker_reopens_durable_retries_under_three_real_slots() {
    if let Some(directory) = std::env::var_os(DIRECTORY) {
        run_live_worker(Path::new(&directory), &std::env::var(ENDPOINT).unwrap());
        return;
    }
    run_live_network_fixture();
}

#[allow(
    clippy::too_many_lines,
    reason = "one HTTPS fixture observes the complete original-exchange retry timeline"
)]
fn run_live_network_fixture() {
    let root = tempfile::tempdir().unwrap();
    let providers = root.path().join("providers");
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let certificate = root.path().join("root.der");
    std::fs::write(&certificate, cert.der()).unwrap();
    let config = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der())),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let (events, requests) = mpsc::channel();
    let stopped = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let server = {
        let stopped = Arc::clone(&stopped);
        let active = Arc::clone(&active);
        let peak = Arc::clone(&peak);
        let providers = providers.clone();
        thread::spawn(move || {
            serve(
                &listener, &config, &events, &stopped, &active, &peak, &providers,
            );
        })
    };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env(DIRECTORY, &providers)
        .env(ENDPOINT, endpoint)
        .env("WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE", certificate)
        .env_remove("WWC_DEVICE_PROVIDER_HTTPS_PROXY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut originals = BTreeMap::new();
    for marker in ["retry", "cancel", "authority"] {
        let request = match requests.recv_timeout(WAIT) {
            Ok(request) => request,
            Err(error) => {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                stopped.store(true, Ordering::Release);
                let server_result = server.join();
                let retained = root.keep();
                panic!(
                    "original actual retry failure request {marker}: {error}; retained at {} (server completed={}): {} {}",
                    retained.display(),
                    server_result.is_ok(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
            }
        };
        assert_eq!(request.1, marker);
        request.3.send(false).unwrap();
        originals.insert(marker, request.2);
    }
    let mut held = BTreeMap::new();
    for _ in 0..3 {
        let request = requests
            .recv_timeout(WAIT)
            .expect("three actual live slot holders");
        assert!(request.1.starts_with("live-"));
        held.insert(request.1, request.3);
    }
    let ready = providers.join("waiting-reopened-retries-checked");
    let started = Instant::now();
    while !ready.exists() && started.elapsed() < WAIT {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    if !ready.exists() {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        stopped.store(true, Ordering::Release);
        for (_, release) in held {
            let _ = release.send(true);
        }
        let server_result = server.join();
        let retained = root.keep();
        panic!(
            "live retry entry retained at {} (server completed={}): {} {}",
            retained.display(),
            server_result.is_ok(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        requests.try_recv().is_err(),
        "reopened entries cannot invoke a fourth request"
    );
    assert_eq!(active.load(Ordering::Acquire), 3);
    held.remove("live-0").unwrap().send(true).unwrap();
    let retry = requests
        .recv_timeout(WAIT)
        .expect("original exchange reacquires released slot");
    assert_eq!(retry.1, "retry");
    assert_eq!(
        retry.2, originals["retry"],
        "fresh Store uses the exact prepared input"
    );
    retry.3.send(true).unwrap();
    for (_, release) in held {
        release.send(true).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    stopped.store(true, Ordering::Release);
    let server_result = server.join();
    if !output.status.success() || server_result.is_err() {
        let retained = root.keep();
        panic!(
            "live retry completion retained at {} (server completed={}): {} {}",
            retained.display(),
            server_result.is_ok(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    server_result.unwrap();
    assert_child(&output);
    assert_eq!(peak.load(Ordering::Acquire), 3);
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert!(
        requests.try_recv().is_err(),
        "cancelled and authority-ended original retries sent no HTTP"
    );
}

fn retry_snapshot(
    database: &rusqlite::Connection,
    request: &ModelOpenMessage,
) -> (i64, i64, String, Vec<u8>, String) {
    database.query_row("SELECT r.policy_attempt,r.wake_ms,r.decision,e.prepared_payload,e.request_open FROM model_retry_state r JOIN exchanges e ON e.exchange_id=r.exchange_id WHERE r.exchange_id=?1", [&request.model_exchange_id.0], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).unwrap()
}

fn store_entries(directory: &Path, request: &ModelOpenMessage) -> Vec<String> {
    std::fs::read_to_string(directory.join("intake.log"))
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            line.contains("stage=provider_store_opened")
                && line.contains(&format!("exchange={} ", request.model_exchange_id.0))
        })
        .map(str::to_owned)
        .collect()
}

#[allow(
    clippy::too_many_lines,
    reason = "same live Worker verifies original retry identity, authority, cancellation and physical accounting"
)]
fn run_live_worker(directory: &Path, endpoint: &str) {
    let _ = ProviderRuntimeStore::prepare(directory).unwrap();
    let mut models = DeviceModels::for_test(directory);
    models.set_intake_log(Some(directory.join("intake.log")));
    let database = crate::device_model::concurrency_tests::runtime_database(directory);
    let config = serde_json::json!({"providerId":"qwen","displayName":"fixture","endpoint":format!("{endpoint}/qwen"),"protocol":"openai_chat_completions","modelIds":["qwen-model"],"enabled":true});
    database
        .execute(
            "INSERT INTO configuration.providers VALUES (?1,?2,?3)",
            rusqlite::params!["qwen", config.to_string(), b"fixture-only-key".as_slice()],
        )
        .unwrap();
    let retry = open("qwen", RETRY, "retry");
    let cancel = open("qwen", CANCEL, "cancel");
    let authority = open("qwen", AUTHORITY, "authority");
    let authorized = Arc::new(AtomicBool::new(true));
    let mut snapshots = BTreeMap::new();
    for request in [&retry, &cancel, &authority] {
        let current = Arc::clone(&authorized);
        let guard: winwincode_codex::LocalModelStartGuard =
            if request.model_exchange_id == authority.model_exchange_id {
                Arc::new(move || current.load(Ordering::Acquire))
            } else {
                Arc::new(|| true)
            };
        assert_eq!(
            models
                .send(
                    &ExecutionPortMessage::ModelOpenMessage(request.clone()),
                    None,
                    Some(guard)
                )
                .unwrap(),
            DeviceModelSendOutcome::Handled
        );
        wait_until(|| {
            let _ = models.next_chunk().unwrap();
            models
                .pending
                .get(&request.model_exchange_id.0)
                .is_some_and(|pending| {
                    pending.thread.is_none() && pending.retry_wake_millis.is_some()
                })
        });
        snapshots.insert(
            request.model_exchange_id.0.clone(),
            retry_snapshot(&database, request),
        );
        assert_eq!(attempts(&database, request), vec!["failed"]);
        assert_eq!(store_entries(directory, request).len(), 1);
        let owner_path = models.directory.join("model-exchange-owners").join(format!(
            "{:x}",
            Sha256::digest(request.model_exchange_id.0.as_bytes())
        ));
        let owner = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(owner_path)
            .unwrap();
        owner
            .try_lock()
            .expect("old provider entry really released its OS exchange owner");
        drop(owner);
    }
    for (member, index) in LIVE.into_iter().enumerate() {
        send(&mut models, &open("qwen", index, &format!("live-{member}")));
    }
    wait_until(|| (0..3).all(|member| directory.join(format!("request-live-{member}")).exists()));
    wait_until(|| {
        let _ = models.next_chunk().unwrap();
        [&retry, &cancel, &authority]
            .into_iter()
            .all(|request| store_entries(directory, request).len() == 2)
    });
    for request in [&retry, &cancel, &authority] {
        assert_eq!(
            retry_snapshot(&database, request),
            snapshots[&request.model_exchange_id.0]
        );
        assert_eq!(attempts(&database, request), vec!["failed"]);
        let entries = store_entries(directory, request);
        assert!(entries[0].contains("retry_entry=false"));
        assert!(entries[1].contains("retry_entry=true"));
        let thread_id = |line: &str| {
            line.split_whitespace()
                .find(|field| field.starts_with("provider_thread="))
                .unwrap()
                .to_owned()
        };
        assert_ne!(thread_id(&entries[0]), thread_id(&entries[1]));
    }
    let store = ProviderRuntimeStore::prepare(directory).unwrap();
    assert!(matches!(
        store.try_model_attempt_permit(&retry).unwrap(),
        DeviceModelAdmission::Deferred
    ));
    store.cancel_model(&cancel.model_exchange_id.0).unwrap();
    authorized.store(false, Ordering::Release);
    let mut authority_failure = false;
    wait_until(|| {
        if let Some(chunk) = models.next_chunk().unwrap() {
            assert_eq!(chunk.model_exchange_id, authority.model_exchange_id);
            assert_eq!(chunk.session_identity, authority.session_identity);
            assert_eq!(chunk.lease, authority.lease);
            assert!(chunk.is_final);
            assert_eq!(
                chunk.error.unwrap(),
                winwincode_execution_port::generated::ExecutionPortError {
                    code:
                        winwincode_execution_port::generated::ExecutionPortErrorCode::LeaseExpired,
                    message: "LEASE_EXPIRED".to_owned(),
                    retryable: false,
                }
            );
            authority_failure = true;
        }
        authority_failure
            && !models.pending.contains_key(&cancel.model_exchange_id.0)
            && !models.pending.contains_key(&authority.model_exchange_id.0)
    });
    assert_eq!(attempts(&database, &cancel), vec!["failed"]);
    assert_eq!(attempts(&database, &authority), vec!["failed"]);
    std::fs::write(
        directory.join("waiting-reopened-retries-checked"),
        b"same-live-worker-checked",
    )
    .unwrap();
    let expected = LIVE
        .into_iter()
        .chain([RETRY])
        .map(|index| format!("mex_{index:026}"))
        .collect::<BTreeSet<_>>();
    let mut completed = BTreeSet::new();
    wait_until(|| {
        if let Some(chunk) = models.next_chunk().unwrap() {
            assert!(chunk.error.is_none());
            if chunk.is_final {
                completed.insert(chunk.model_exchange_id.0);
            }
        }
        completed == expected
    });
    assert_eq!(attempts(&database, &retry), vec!["failed", "completed"]);
    let prepared: Vec<u8> = database
        .query_row(
            "SELECT prepared_payload FROM exchanges WHERE exchange_id=?1",
            [&retry.model_exchange_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(prepared, snapshots[&retry.model_exchange_id.0].3);
    assert_eq!(store_entries(directory, &retry).len(), 2);
    models.log_intake(IntakeLogLine {
        stage: "focused_complete",
        code: "OK",
        exchange_id: &retry.model_exchange_id.0,
        sequence: 0,
        worker_session_id: &retry.worker_session_id.0,
        thread_id: &retry.session_identity.codex_thread_id.0,
        detail: "same_live_worker=true",
    });
}
