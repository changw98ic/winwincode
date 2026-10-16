// SPDX-License-Identifier: Apache-2.0

//! Real Device subprocess death preserves inline `JEv` preparation and physical charges.
//! Coverage is Device/Provider recovery; live Core and Worker recovery has a separate gate.

use super::*;
use std::{
    fs,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    os::unix::{fs::DirBuilderExt as _, process::ExitStatusExt as _},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use winwincode_execution_port::{
    agent_config::{AgentProfileSettings, resolve_agent_session_config},
    generated::{
        ExecutionJob, ExecutionScope, ProductSessionExecutionScope,
        ProductSessionExecutionScopeKind,
    },
};

const ROOT_ENV: &str = "WWC_JEV_RECOVERY_TEST_ROOT";
const CASE_ENV: &str = "WWC_JEV_RECOVERY_TEST_CASE";
const PHASE_ENV: &str = "WWC_JEV_RECOVERY_TEST_PHASE";
const TEST_NAME: &str = "device_model::jev_recovery_tests::device_jev_preparation_death_and_prepared_retry_preserve_original_paid_facts";
const WAIT: Duration = Duration::from_secs(15);
type WireRequests = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

#[derive(Debug, PartialEq)]
struct ReceiptRow {
    kind: String,
    operation_id: String,
    digest: Option<String>,
    request: String,
    result: Option<String>,
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

#[test]
fn device_jev_preparation_death_and_prepared_retry_preserve_original_paid_facts() {
    if let Ok(root) = std::env::var(ROOT_ENV) {
        run_child(Path::new(&root));
        return;
    }
    for case in ["preparation", "retry"] {
        exercise_case(case);
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep subprocess death and byte-exact paid recovery observations in one lifecycle"
)]
fn exercise_case(case: &str) {
    let root = std::env::temp_dir().join(format!(
        "wwc-jev-recovery-{case}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    fs::write(root.join("root.der"), certified.cert.der()).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![certified.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let open = configure_exchange(&root, &origin);
    fs::write(root.join("open.json"), serde_json::to_vec(&open).unwrap()).unwrap();
    let requests: WireRequests = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(AtomicBool::new(false));
    let release_stalled = Arc::new(AtomicBool::new(false));
    let (wire, stopped, release) = (
        Arc::clone(&requests),
        Arc::clone(&done),
        Arc::clone(&release_stalled),
    );
    let preparing = case == "preparation";
    let server = thread::spawn(move || {
        serve(
            &listener,
            &Arc::new(tls),
            &wire,
            &stopped,
            &release,
            preparing,
        );
    });
    let database = rusqlite::Connection::open_with_flags(
        root.join("runtime/provider-runtime.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    database.busy_timeout(Duration::from_secs(5)).unwrap();
    let mut initial = spawn_child(&root, case, "initial");
    wait_until(
        || {
            if preparing {
                let receipts = receipt_rows(&database);
                receipts.len() == 2
                    && receipts.iter().filter(|row| row.result.is_some()).count() == 1
                    && requests.lock().unwrap().len() == 2
            } else {
                root.join("deferred-ready").exists()
            }
        },
        "real paid preparation/deferred cut point",
    );
    let retry_before = if preparing {
        None
    } else {
        Some(retry_snapshot(&database, &open))
    };
    if let Some(retry) = &retry_before {
        assert_eq!(retry["attempt"], 2);
        assert_eq!(retry["decision"], "deferred_until");
        fs::write(root.join("retry.json"), serde_json::to_vec(retry).unwrap()).unwrap();
    }
    let physical_before = table_rows(
        &database,
        &open,
        "SELECT * FROM model_invocation_attempts WHERE exchange_id=?1 AND attempt_number=1",
    );
    let diagnostic_before = table_rows(
        &database,
        &open,
        "SELECT * FROM model_attempt_diagnostics WHERE exchange_id=?1 AND sequence=1",
    );
    assert_eq!(physical_before.len(), usize::from(!preparing));
    assert_eq!(diagnostic_before.len(), usize::from(!preparing));
    let receipts_before = receipt_rows(&database);
    assert_eq!(receipts_before.len(), 2);
    assert_eq!(
        receipts_before
            .iter()
            .map(|row| row.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["context", "judge"]
    );
    for row in receipts_before.iter().filter(|row| row.result.is_some()) {
        let run: serde_json::Value = serde_json::from_str(row.result.as_ref().unwrap()).unwrap();
        assert_eq!(run["observation"]["inputTokens"], 7);
        assert_eq!(run["observation"]["outputTokens"], 2);
        assert_eq!(run["failures"], serde_json::json!([]));
    }
    if preparing {
        assert!(receipts_before[0].result.is_none());
        assert!(receipts_before[1].result.is_some());
    }
    assert_eq!(
        receipts_before
            .iter()
            .filter(|row| row.result.is_some())
            .count(),
        if preparing { 1 } else { 2 }
    );
    let original_request: String = database
        .query_row(
            "SELECT request_open FROM exchanges WHERE exchange_id=?1",
            [&open.model_exchange_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<ModelOpenMessage>(&original_request).unwrap(),
        open
    );
    let prepared_before = prepared(&database, &open);
    assert_eq!(prepared_before.is_some(), !preparing);
    if let Some((bytes, checksum)) = &prepared_before {
        assert_eq!(checksum, &format!("sha256:{:x}", Sha256::digest(bytes)));
    }
    initial.0.kill().unwrap();
    assert_eq!(
        initial.0.wait().unwrap().signal(),
        Some(9),
        "the Device owner must die before recovery"
    );
    release_stalled.store(true, Ordering::Release);
    let mut resumed = spawn_child(&root, case, "resume");
    wait_until(
        || resumed.0.try_wait().unwrap().is_some(),
        "same-exchange Device recovery",
    );
    assert!(resumed.0.wait().unwrap().success());
    let chunks: Vec<ModelChunkMessage> =
        serde_json::from_slice(&fs::read(root.join("resumed.json")).unwrap()).unwrap();
    assert!(chunks.iter().all(|chunk| chunk.lease == open.lease
        && chunk.worker_session_id == open.worker_session_id
        && chunk.session_identity == open.session_identity
        && chunk.model_exchange_id == open.model_exchange_id));
    if preparing {
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].error.as_ref().unwrap().code,
            ExecutionPortErrorCode::DeviceModelInterrupted
        );
    } else {
        assert!(
            chunks
                .last()
                .is_some_and(|chunk| chunk.is_final && chunk.error.is_none())
        );
    }
    assert_eq!(
        receipt_rows(&database),
        receipts_before,
        "reopen must preserve completed and unknown JEv receipts exactly"
    );
    assert_eq!(prepared(&database, &open), prepared_before);
    assert_eq!(
        database
            .query_row(
                "SELECT request_open FROM exchanges WHERE exchange_id=?1",
                [&open.model_exchange_id.0],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        original_request
    );
    assert_eq!(
        table_rows(
            &database,
            &open,
            "SELECT * FROM model_invocation_attempts WHERE exchange_id=?1 AND attempt_number=1"
        ),
        physical_before
    );
    assert_eq!(
        table_rows(
            &database,
            &open,
            "SELECT * FROM model_attempt_diagnostics WHERE exchange_id=?1 AND sequence=1"
        ),
        diagnostic_before
    );
    let retry_after: i64 = database
        .query_row(
            "SELECT count(*) FROM model_retry_state WHERE exchange_id=?1",
            [&open.model_exchange_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retry_after, 0);
    let physical: Vec<(i64,String)> = database.prepare("SELECT attempt_number,state FROM model_invocation_attempts WHERE exchange_id=?1 ORDER BY attempt_number")
        .unwrap().query_map([&open.model_exchange_id.0], |row| Ok((row.get(0)?,row.get(1)?))).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(
        physical,
        if preparing {
            Vec::new()
        } else {
            vec![(1, "failed".into()), (2, "completed".into())]
        }
    );
    drop(database);
    let store = DeviceProviderStore::open(&root).unwrap();
    assert_eq!(
        store.execute_model_authorized(&open, || false).unwrap(),
        chunks,
        "completed logical exchange replays without new authorization or HTTP"
    );

    let statement = store.accounting_statement(&open.lease).unwrap().unwrap();
    statement.validate().unwrap();
    let primary_slot = format!("primary:{}", open.model_exchange_id.0);
    let mut expected_manifest = receipts_before
        .iter()
        .map(|row| row.operation_id.clone())
        .collect::<Vec<_>>();
    expected_manifest.push(primary_slot.clone());
    expected_manifest.sort();
    assert_eq!(statement.manifest, expected_manifest);
    let mut measured_slots = receipts_before
        .iter()
        .filter(|row| row.result.is_some())
        .map(|row| row.operation_id.clone())
        .collect::<Vec<_>>();
    if !preparing {
        measured_slots.push(primary_slot.clone());
    }
    measured_slots.sort();
    assert_eq!(
        statement
            .receipts
            .iter()
            .map(|receipt| receipt.slot.clone())
            .collect::<Vec<_>>(),
        measured_slots,
        "only actual retained measurements may become financial receipts"
    );
    for receipt in &statement.receipts {
        assert_eq!(receipt.model_exchange_id, open.model_exchange_id);
        if receipt.slot == primary_slot {
            assert_eq!(
                receipt.tokens, None,
                "the first sent 503 attempt has unknown usage"
            );
            assert_eq!(receipt.known_tokens, 5);
        } else {
            assert_eq!(receipt.tokens, Some(9));
            assert_eq!(receipt.known_tokens, 9);
            assert_eq!(receipt.provider_receipt_id, receipt.slot);
        }
    }
    if preparing {
        let incomplete = receipts_before
            .iter()
            .find(|row| row.result.is_none())
            .unwrap();
        assert!(statement.manifest.contains(&incomplete.operation_id));
        assert!(
            !statement
                .receipts
                .iter()
                .any(|receipt| receipt.slot == incomplete.operation_id)
        );
        assert!(
            !statement
                .receipts
                .iter()
                .any(|receipt| receipt.slot == primary_slot)
        );
    }
    assert_eq!(
        statement
            .receipts
            .iter()
            .map(|receipt| receipt.tokens.unwrap_or(receipt.known_tokens))
            .sum::<u64>(),
        if preparing { 9 } else { 23 }
    );
    assert!(
        statement
            .receipts
            .iter()
            .all(|receipt| receipt.cost_microunits.is_none())
    );
    assert_eq!(
        store.accounting_statement(&open.lease,).unwrap(),
        Some(statement)
    );
    drop(store);
    done.store(true, Ordering::Release);
    server.join().unwrap();
    let wire = requests.lock().unwrap();
    let jev: Vec<_> = wire.iter().filter(|(path, _)| path == "/jev").collect();
    let primary: Vec<_> = wire.iter().filter(|(path, _)| path == "/model").collect();
    assert_eq!(
        jev.len(),
        2,
        "death and reopen must add no paid preparation request"
    );
    assert_eq!(primary.len(), if preparing { 0 } else { 2 });
    if !preparing {
        assert_eq!(
            primary[0].1, primary[1].1,
            "deferred retry sends the exact originally prepared input"
        );
        let prepared_bytes = prepared_before.unwrap().0;
        assert_eq!(
            primary[0].1, prepared_bytes,
            "the canonical adapter sends the entire original prepared envelope"
        );
        let request: serde_json::Value = serde_json::from_slice(&prepared_bytes).unwrap();
        let sent: serde_json::Value = serde_json::from_slice(&primary[0].1).unwrap();
        assert_eq!(sent, request);
        for field in [
            "winwincodeJevContext",
            "winwincodeJevTask",
            "winwincodeJevJudge",
        ] {
            assert!(request.get(field).is_none());
        }
        assert_eq!(
            request["request"]["instructions"],
            "Retain the original Chat instructions"
        );
        let original: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&open.request.data_base64).unwrap()).unwrap();
        for item in original["request"]["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| {
                item["role"] == "user"
                    || ["custom_tool_call", "custom_tool_call_output"]
                        .contains(&item["type"].as_str().unwrap_or(""))
            })
        {
            assert!(
                request["request"]["input"]
                    .as_array()
                    .unwrap()
                    .contains(item)
            );
        }
    }
    drop(wire);
    fs::remove_dir_all(root).unwrap();
}

fn spawn_child(root: &Path, case: &str, phase: &str) -> OwnedChild {
    OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(ROOT_ENV, root)
            .env(CASE_ENV, case)
            .env(PHASE_ENV, phase)
            .env(
                "WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE",
                root.join("root.der"),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}

fn run_child(root: &Path) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let case = std::env::var(CASE_ENV).unwrap();
    let phase = std::env::var(PHASE_ENV).unwrap();
    let open: ModelOpenMessage =
        serde_json::from_slice(&fs::read(root.join("open.json")).unwrap()).unwrap();
    let store = DeviceProviderStore::open(root).unwrap();
    if case == "retry" && phase == "resume" {
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("retry.json")).unwrap()).unwrap();
        assert_eq!(retry_snapshot(&store.runtime.connection, &open), original);
        assert_eq!(original["attempt"], 2);
        assert_eq!(original["decision"], "deferred_until");
        // Advance only the fixture wake time. Preserve the real durable retry
        // decision, original exchange, prepared input and policy attempt.
        store
            .runtime
            .connection
            .execute(
                "UPDATE model_retry_state SET wake_ms=0 WHERE exchange_id=?1",
                [&open.model_exchange_id.0],
            )
            .unwrap();
    }
    let can_start = || {
        if case == "retry"
            && phase == "initial"
            && store.model_retry_state(&open).unwrap().is_some()
        {
            fs::write(root.join("deferred-ready"), b"durable retry retained").unwrap();
            loop {
                thread::sleep(Duration::from_millis(10));
            }
        }
        true
    };
    let chunks = store
        .execute_model_recovering_authorized(
            &open,
            can_start,
            || store.wait_model_attempt_permit(&open, &can_start),
            || {},
        )
        .unwrap();
    assert_eq!(
        phase, "resume",
        "the parent must kill initial preparation/deferral before it returns"
    );
    assert_eq!(
        store.execute_model_authorized(&open, || false).unwrap(),
        chunks
    );
    fs::write(
        root.join("resumed.json"),
        serde_json::to_vec(&chunks).unwrap(),
    )
    .unwrap();
}

#[allow(
    clippy::too_many_lines,
    reason = "Seal the original ordinary Chat task and both JEv routes for real recovery"
)]
fn configure_exchange(root: &Path, origin: &str) -> ModelOpenMessage {
    let store = DeviceProviderStore::open(root).unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let messages = fixture["messages"].as_array().unwrap();
    let mut open: ModelOpenMessage = serde_json::from_value(
        messages
            .iter()
            .find(|item| item["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let mut job: ExecutionJob = serde_json::from_value(
        messages
            .iter()
            .find(|item| item["kind"] == "job.dispatch")
            .unwrap()["job"]
            .clone(),
    )
    .unwrap();
    job.job_id = open.lease.job_id.clone();
    job.scope = ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
        kind: ProductSessionExecutionScopeKind::ProductSession,
        product_session_id: open.session_identity.product_session_id.clone(),
    });
    job.work_input = None;
    open.session_identity.work_run_id = None;
    let profile: AgentProfileSettings = serde_json::from_value(serde_json::json!({
        "provider":"jev-recovery-primary","model":"fixture-model","reasoning":"max","tools":[],"sandbox":"candidate","instructions":null,
        "jevJudge":"jev-recovery-paid","jevContext":{"provider":"jev-recovery-paid","policy":{"version":"jev-recovery-v1",
            "minimumConfidence":0.6,"pinThreshold":0.9,"keepThreshold":0.9,"compactThreshold":0.9,"dropThreshold":0.9,
            "taskMemoryThreshold":0.5,"projectMemoryThreshold":0.7,"longTermMemoryThreshold":0.9}}
    })).unwrap();
    let session = resolve_agent_session_config(&open.lease.worker_id,
        &serde_json::from_value(serde_json::json!({"capabilityDigest":format!("sha256:{}","a".repeat(64)),"features":[],"maxConcurrentJobs":1,"platform":"aarch64-apple-darwin"})).unwrap(),
        "default", profile).unwrap();
    let payload = serde_json::to_vec(&serde_json::json!({"provider":"jev-recovery-primary",
        "winwincodeJevContext":session,"winwincodeJevTask":{"job":job,"jobDigest":format!("sha256:{:x}",Sha256::digest(serde_json::to_vec(&job).unwrap()))},
        "winwincodeJevJudge":[{"claimKey":"original-command","premise":"Retained tool reported success","hypothesis":"The original command succeeded"}],
        "request":{"model":"fixture-model","stream":true,"instructions":"Retain the original Chat instructions","input":[
            {"type":"message","role":"user","content":"Check the retained command evidence"},
            {"type":"custom_tool_call","name":"exec","call_id":"original-tool","input":"text('original fact')"},
            {"type":"custom_tool_call_output","call_id":"original-tool","output":"original fact"},
            {"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Earlier original progress"}]},
            {"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Latest original progress"}]}
        ]}})).unwrap();
    open.request.data_base64 = STANDARD.encode(&payload);
    open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(&payload));
    open.request.content_type = "application/json".into();
    let provider = serde_json::json!({"providerId":"jev-recovery-primary","displayName":"Owned recovery fixture","endpoint":format!("{origin}/model"),"protocol":"canonical","modelIds":["fixture-model"],"enabled":true});
    store
        .connection
        .execute(
            "INSERT INTO providers VALUES(?1,?2,?3)",
            params![
                "jev-recovery-primary",
                provider.to_string(),
                b"owned-primary-secret".as_slice()
            ],
        )
        .unwrap();
    let settings: crate::OpenJevRemoteSettings = serde_json::from_value(serde_json::json!({"providerId":"jev-recovery-paid","endpoint":format!("{origin}/jev"),"apiKey":"owned-jev-secret","modelId":"fixture-model","timeoutMs":10_000,"retries":0,"maxBatchSize":8,"devices":["remote"],"dtypes":["auto"]})).unwrap();
    store.save_jev_settings(&settings).unwrap();
    open
}

fn retry_snapshot(database: &rusqlite::Connection, open: &ModelOpenMessage) -> serde_json::Value {
    database.query_row("SELECT policy_attempt,wake_ms,decision FROM model_retry_state WHERE exchange_id=?1",
        [&open.model_exchange_id.0], |row| Ok(serde_json::json!({
            "attempt":row.get::<_,i64>(0)?, "wakeMs":row.get::<_,i64>(1)?, "decision":row.get::<_,String>(2)?
        }))).unwrap()
}

// Preserve every column and its SQLite storage type. BLOBs retain exact bytes,
// so adding a new physical attempt cannot hide mutation of the first receipt.
fn table_rows(
    database: &rusqlite::Connection,
    open: &ModelOpenMessage,
    sql: &str,
) -> Vec<serde_json::Value> {
    use rusqlite::types::ValueRef;
    let mut statement = database.prepare(sql).unwrap();
    let names: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    statement
        .query_map([&open.model_exchange_id.0], |row| {
            let mut columns = serde_json::Map::new();
            for (index, name) in names.iter().enumerate() {
                let value = match row.get_ref(index)? {
                    ValueRef::Null => serde_json::json!({"type":"null"}),
                    ValueRef::Integer(value) => serde_json::json!({"type":"integer","value":value}),
                    ValueRef::Real(value) => serde_json::json!({"type":"real","value":value}),
                    ValueRef::Text(value) => {
                        serde_json::json!({"type":"text","bytes":STANDARD.encode(value)})
                    }
                    ValueRef::Blob(value) => {
                        serde_json::json!({"type":"blob","bytes":STANDARD.encode(value)})
                    }
                };
                columns.insert(name.clone(), value);
            }
            Ok(serde_json::Value::Object(columns))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn receipt_rows(database: &rusqlite::Connection) -> Vec<ReceiptRow> {
    database.prepare("SELECT 'context',operation_id,digest,request_json,result FROM jev_context_exchanges UNION ALL SELECT 'judge',operation_id,NULL,request_json,result FROM jev_judge_exchanges ORDER BY 1,2")
        .unwrap().query_map([], |row|Ok(ReceiptRow { kind:row.get(0)?,operation_id:row.get(1)?,digest:row.get(2)?,request:row.get(3)?,result:row.get(4)? })).unwrap().collect::<Result<_,_>>().unwrap()
}

fn prepared(database: &rusqlite::Connection, open: &ModelOpenMessage) -> Option<(Vec<u8>, String)> {
    database.query_row("SELECT prepared_payload,prepared_payload_digest FROM exchanges WHERE exchange_id=?1 AND prepared_payload IS NOT NULL", [&open.model_exchange_id.0], |row|Ok((row.get(0)?,row.get(1)?))).optional().unwrap()
}

fn wait_until(mut observed: impl FnMut() -> bool, description: &str) {
    let deadline = Instant::now() + WAIT;
    while !observed() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn serve(
    listener: &TcpListener,
    tls: &Arc<rustls::ServerConfig>,
    requests: &WireRequests,
    done: &AtomicBool,
    release: &AtomicBool,
    preparation: bool,
) {
    let mut jev_calls = 0;
    let mut primary_calls = 0;
    while !done.load(Ordering::Acquire) {
        let socket = match listener.accept() {
            Ok((socket, _)) => socket,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(error) => panic!("owned HTTPS recovery accept: {error}"),
        };
        socket.set_nonblocking(false).unwrap();
        socket.set_read_timeout(Some(WAIT)).unwrap();
        socket.set_write_timeout(Some(WAIT)).unwrap();
        let mut stream = rustls::StreamOwned::new(
            rustls::ServerConnection::new(Arc::clone(tls)).unwrap(),
            socket,
        );
        let (path, bytes) = read_request(&mut stream);
        requests.lock().unwrap().push((path.clone(), bytes.clone()));
        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        if path == "/jev" {
            jev_calls += 1;
            if preparation && jev_calls == 2 {
                while !release.load(Ordering::Acquire) && !done.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                continue;
            }
            let answers: serde_json::Map<String,serde_json::Value> = payload["questions"].as_object().unwrap().keys().map(|key|(key.clone(),serde_json::json!({"type":"choice","choice":"neutral","confidence":0.8,"probabilities":{"entailment":0.1,"contradiction":0.1,"neutral":0.8}}))).collect();
            let body=serde_json::json!({"model":"fixture-model","answers":answers,"usage":{"input_tokens":7,"output_tokens":2}}).to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        } else {
            assert_eq!(path, "/model");
            primary_calls += 1;
            if primary_calls == 1 {
                stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nRetry-After: 301\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
            } else {
                let body = concat!(
                    "data: {\"type\":\"response.started\",\"responseId\":\"jev-recovered-response\",\"modelId\":\"fixture-model\"}\n\n",
                    "data: {\"type\":\"usage\",\"inputTokens\":3,\"outputTokens\":2,\"cacheWriteInputTokens\":0,\"reasoningOutputTokens\":0}\n\n",
                    "data: {\"type\":\"response.finished\",\"reason\":\"stop\"}\n\n"
                );
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        }
        stream.flush().unwrap();
    }
}

fn read_request(
    stream: &mut rustls::StreamOwned<rustls::ServerConnection, TcpStream>,
) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0);
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() <= 2 * 1024 * 1024);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            if bytes.len() >= end + 4 + length {
                let path = headers
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                return (path, bytes[end + 4..end + 4 + length].to_vec());
            }
        }
    }
}
