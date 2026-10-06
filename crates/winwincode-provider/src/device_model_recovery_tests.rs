// SPDX-License-Identifier: Apache-2.0

//! Real HTTPS requests exercise recovery, durable receipts, replay and cancellation.

use super::*;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use std::{
    cell::Cell,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use winwincode_execution_port::action_enforcement::ActionEnforcementSigningKey;

type DeviceAttemptRow = (i64, String, Option<String>, Option<Vec<u8>>);

const CHILD_CASE: &str = "WWC_MODEL_RECOVERY_TEST_CASE";
const CHILD_ROOT: &str = "WWC_MODEL_RECOVERY_TEST_ROOT";
const CHILD_ENDPOINT: &str = "WWC_MODEL_RECOVERY_TEST_ENDPOINT";
const TEST_NAME: &str = "device_model::recovery_tests::real_provider_attempts_recover_without_replaying_tools_or_network";

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one canonical fixture verifies preserved runtime bytes and paid facts across every failure category"
)]
fn canonical_failure_categories_and_paid_cost_survive_attempt_recovery() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let open: ModelOpenMessage = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let usage = crate::ProviderTokenUsage {
        input_tokens: 3,
        output_tokens: 2,
        cached_input_tokens: None,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 0,
    };
    for (kind, code, status) in [
        (
            crate::ProviderStreamFailureKind::Authentication,
            "AUTH",
            401,
        ),
        (crate::ProviderStreamFailureKind::Quota, "QUOTA", 429),
        (
            crate::ProviderStreamFailureKind::InvalidRequest,
            "CONTENT_FILTER",
            400,
        ),
        (
            crate::ProviderStreamFailureKind::ContextWindowExceeded,
            "CONTEXT_WINDOW_EXCEEDED",
            400,
        ),
        (
            crate::ProviderStreamFailureKind::RateLimit,
            "RATE_LIMIT",
            429,
        ),
    ] {
        let receipt = ProviderGatewayOpenReceipt {
            model_exchange_id: open.model_exchange_id.clone(),
            request_id: open.request_id.clone(),
            route: ModelRoute {
                provider_id: "fixture-provider".into(),
                model_id: "fixture-model".into(),
                credential_reference_id: CredentialReferenceId(
                    "crd_00000000000000000000000001".into(),
                ),
            },
            adapter_request_id: "fixture-attempt".into(),
            idempotent_replay: false,
            stream_leak_gate: CredentialLeakGate::new(),
        };
        let mut converter = crate::ProviderStreamConverter::from_gateway_receipt(&receipt);
        let mut frames = converter
            .ingest(crate::ProviderStreamEvent::ResponseStarted {
                provider_response_id: "actual-response".into(),
                observed_model_id: None,
            })
            .unwrap();
        frames.extend(
            converter
                .ingest(crate::ProviderStreamEvent::Usage(usage))
                .unwrap(),
        );
        let terminal_event = if code == "CONTENT_FILTER" {
            crate::ProviderStreamEvent::Finished(crate::ProviderFinishReason::Filtered)
        } else {
            crate::ProviderStreamEvent::Failed(
                crate::ProviderStreamFailure::new(kind)
                    .with_status(status)
                    .with_retry_after_millis(2_500),
            )
        };
        frames.extend(converter.ingest(terminal_event).unwrap());
        let original = frames.last().unwrap().payload_json().to_owned();
        let completion = crate::HttpsSseProviderCompletion {
            frames,
            terminal: crate::ProviderGatewayTerminal::Failed {
                failure: crate::ModelAttemptFailureFact::from_stream(
                    kind,
                    crate::ModelExecutionCertainty::AcceptanceUnknown,
                ),
                charge: Some(crate::ProviderGatewayTerminalCharge {
                    usage,
                    actual_cost_micros: Some(7),
                }),
            },
        };
        let outcome = completion_outcome(&open, &completion).unwrap();
        let accounting = if kind == crate::ProviderStreamFailureKind::RateLimit {
            let ModelAttemptOutcome::Failed {
                chunk,
                accounting,
                retry_after,
                retryable,
                ..
            } = outcome
            else {
                panic!("real limit remains recoverable");
            };
            assert!(retryable);
            assert_eq!(retry_after, Some(2_500));
            let metadata: serde_json::Value = serde_json::from_slice(
                &STANDARD
                    .decode(&chunk.payload.as_ref().unwrap().data_base64)
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(metadata["status"], status);
            accounting.unwrap()
        } else {
            let ModelAttemptOutcome::TerminalFailure(chunks, accounting) = outcome else {
                panic!("nontransient categories remain canonical");
            };
            let decoded = STANDARD
                .decode(&chunks.last().unwrap().payload.as_ref().unwrap().data_base64)
                .unwrap();
            assert_eq!(decoded, original.as_bytes());
            let value: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
            assert_eq!(value["error"]["code"], code);
            accounting.unwrap()
        };
        let value: serde_json::Value = serde_json::from_slice(
            &STANDARD
                .decode(&accounting.payload.as_ref().unwrap().data_base64)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(value["actualCostMicros"], 7);
        assert_eq!(value["tokenUsage"]["input_tokens"], 3);
        assert_eq!(value["tokenUsage"]["output_tokens"], 2);
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one durable crash fixture verifies owner liveness, restart, cancellation and financial closure together"
)]
fn interrupted_and_cancelled_attempts_close_with_honest_financial_facts() {
    for original_state in [
        "prepared",
        "invoking",
        "cancelled_invoking",
        "completed_before_outer",
    ] {
        let root = std::env::temp_dir().join(format!(
            "wwc-model-interrupted-{}-{}-{original_state}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = DeviceProviderStore::open(&root).unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut open: ModelOpenMessage = serde_json::from_value(
            fixture["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["kind"] == "model.open")
                .unwrap()
                .clone(),
        )
        .unwrap();
        let payload = serde_json::to_vec(
            &serde_json::json!({"provider":"fixture-provider","request":{"model":"fixture-model"}}),
        )
        .unwrap();
        open.request.content_type = "application/json".into();
        open.request.data_base64 = STANDARD.encode(&payload);
        open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(&payload));
        let request = serde_json::to_string(&open).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO exchanges(exchange_id,digest,request_open) VALUES(?1,?2,?3)",
                params![
                    open.model_exchange_id.0,
                    format!("{:x}", Sha256::digest(request.as_bytes())),
                    request
                ],
            )
            .unwrap();
        let state = if original_state == "prepared" {
            "prepared"
        } else {
            "invoking"
        };
        store.connection.execute("INSERT INTO model_invocation_attempts(exchange_id,attempt_number,adapter_request_id,state) VALUES(?1,1,?2,?3)",params![open.model_exchange_id.0,format!("device-{}:attempt:1",open.model_exchange_id.0),state]).unwrap();
        if original_state == "completed_before_outer" {
            let accounting = observed_accounting_chunk(
                &open,
                "genuine-paid-response",
                crate::ProviderTokenUsage {
                    input_tokens: 3,
                    output_tokens: 2,
                    cached_input_tokens: None,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                },
            )
            .unwrap();
            store
                .connection
                .execute(
                    "UPDATE model_invocation_attempts SET state='completed',accounting_chunks=?1",
                    [serde_json::to_string(&vec![accounting]).unwrap()],
                )
                .unwrap();
            let live = store
                .try_model_exchange_owner(&open.model_exchange_id.0)
                .unwrap()
                .unwrap();
            let key = ActionEnforcementSigningKey::from_bytes([7; 32]).unwrap();
            assert!(
                store
                    .accounting_statement(&open.lease, &key)
                    .unwrap()
                    .is_none(),
                "a live owner still controls the final exchange write"
            );
            drop(live);
            let statement = store
                .accounting_statement(&open.lease, &key)
                .unwrap()
                .unwrap();
            statement.verify(&key).unwrap();
            assert_eq!(
                statement.receipts[0].tokens,
                Some(5),
                "a dead owner cannot strand a genuine completed paid receipt"
            );
            let runtime: Option<String> = store
                .connection
                .query_row("SELECT chunks FROM exchanges", [], |row| row.get(0))
                .unwrap();
            assert!(
                runtime.is_none(),
                "financial recovery never manufactures a completed model result"
            );
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
            continue;
        }
        if original_state == "cancelled_invoking" {
            store.cancel_model(&open.model_exchange_id.0).unwrap();
        }
        let result = store
            .execute_model_recovering_authorized(
                &open,
                || true,
                || panic!("unfinished replay cannot start an actual request"),
                || panic!("no admission was taken"),
            )
            .unwrap();
        if original_state == "cancelled_invoking" {
            assert!(result.is_empty());
        } else {
            assert_eq!(
                result[0].error.as_ref().unwrap().code,
                ExecutionPortErrorCode::DeviceModelInterrupted
            );
        }
        let final_state: String = store
            .connection
            .query_row("SELECT state FROM model_invocation_attempts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            final_state,
            if original_state == "prepared" {
                "not_sent"
            } else {
                "interrupted_unknown"
            }
        );
        let key = ActionEnforcementSigningKey::from_bytes([7; 32]).unwrap();
        let statement = store
            .accounting_statement(&open.lease, &key)
            .unwrap()
            .unwrap();
        statement.verify(&key).unwrap();
        assert_eq!(statement.manifest.len(), 1);
        assert_eq!(statement.receipts.len(), 1);
        assert_eq!(
            statement.receipts[0].tokens,
            if original_state == "prepared" {
                Some(0)
            } else {
                None
            }
        );
        assert_eq!(
            statement.receipts[0].cost_microunits,
            if original_state == "prepared" {
                Some(0)
            } else {
                None
            }
        );
        assert_eq!(
            store.accounting_statement(&open.lease, &key).unwrap(),
            Some(statement)
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one HTTPS process fixture validates every recovery scenario against actual request counts"
)]
fn real_provider_attempts_recover_without_replaying_tools_or_network() {
    if let Ok(case) = std::env::var(CHILD_CASE) {
        run_child(&case);
        return;
    }
    for case in [
        "rate_limit",
        "upstream",
        "rejected",
        "incomplete",
        "cancel_wait",
        "authority_wait",
        "cross_process_cancel",
        "completed_reset",
        "incomplete_reset",
    ] {
        let root = std::env::temp_dir().join(format!(
            "wwc-model-recovery-{}-{}-{case}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        drop(DeviceProviderStore::open(&root).unwrap());
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certificate = root.join("root.der");
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
            "https://localhost:{}/model",
            listener.local_addr().unwrap().port()
        );
        let responses = match case {
            "rate_limit" => vec![
                ("429 Too Many Requests", "Retry-After: 0\r\n", "{}"),
                success(),
            ],
            "upstream" => vec![
                ("503 Service Unavailable", "Retry-After: 0\r\n", "{}"),
                success(),
            ],
            "rejected" => vec![("403 Forbidden", "", "private response body must not escape")],
            "incomplete" => vec![
                (
                    "200 OK",
                    "",
                    concat!(
                        "data: {\"id\":\"paid-incomplete\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
                        "data: {\"id\":\"paid-incomplete\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n"
                    ),
                ),
                success(),
            ],
            "cancel_wait" | "authority_wait" => {
                vec![("429 Too Many Requests", "Retry-After: 60\r\n", "{}")]
            }
            "cross_process_cancel" => vec![success()],
            "completed_reset" => vec![(
                "200 OK",
                "X-Test-Truncated-Length: true\r\n",
                concat!(
                    "data: {\"id\":\"finished-response\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"finished-response\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n"
                ),
            )],
            "incomplete_reset" => vec![
                (
                    "200 OK",
                    "X-Test-Truncated-Length: true\r\n",
                    concat!(
                        "data: {\"id\":\"paid-incomplete\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
                        "data: {\"id\":\"paid-incomplete\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n"
                    ),
                ),
                success(),
            ],
            _ => unreachable!(),
        };
        let count = responses.len();
        let concurrent_root = (case == "cross_process_cancel").then(|| root.clone());
        let server = std::thread::spawn(move || {
            serve(&listener, &config, responses, concurrent_root.as_deref())
        });
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_CASE, case)
            .env(CHILD_ROOT, &root)
            .env(CHILD_ENDPOINT, endpoint)
            .env("WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE", certificate)
            .env_remove("WWC_DEVICE_PROVIDER_HTTPS_PROXY")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "case {case}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), count, "only actual attempts reach HTTPS");
        if count == 2 {
            assert_ne!(
                idempotency_key(&requests[0]),
                idempotency_key(&requests[1]),
                "actual retries must retain distinct charge identities"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the isolated production request checks admission, durable attempt identity, cancellation and exact replay together"
)]
fn run_child(case: &str) {
    let root = std::path::PathBuf::from(std::env::var_os(CHILD_ROOT).unwrap());
    let store = DeviceProviderStore::open(&root).unwrap();
    let config = serde_json::json!({"providerId":"recovery-provider","displayName":"fixture","endpoint":std::env::var(CHILD_ENDPOINT).unwrap(),"protocol":"openai_chat_completions","modelIds":["fixture-model"],"enabled":true});
    store
        .connection
        .execute(
            "INSERT INTO providers VALUES (?1,?2,?3)",
            params![
                "recovery-provider",
                config.to_string(),
                b"private-fixture-key".as_slice()
            ],
        )
        .unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let mut open: ModelOpenMessage = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let payload = serde_json::to_vec(&serde_json::json!({"requestId":open.request_id.0,"provider":"recovery-provider","sessionId":"fixture-session","threadId":"fixture-thread","request":{"model":"fixture-model","instructions":"fixture","input":[{"type":"message","id":"fixture-message","role":"user","content":[{"type":"input_text","text":"fixture"}]}],"tools":[],"tool_choice":"auto","parallel_tool_calls":true,"stream":true,"store":false}})).unwrap();
    open.request.content_type = "application/json".into();
    open.request.data_base64 = STANDARD.encode(&payload);
    open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(&payload));
    let admissions = Cell::new(0);
    let releases = Cell::new(0);
    let authority = Cell::new(true);
    let (sent, received) = std::sync::mpsc::channel();
    let cancellation = if case == "cancel_wait" {
        let root = root.clone();
        let exchange = open.model_exchange_id.0.clone();
        Some(std::thread::spawn(move || {
            received.recv().unwrap();
            std::thread::sleep(Duration::from_millis(30));
            DeviceProviderStore::open(&root)
                .unwrap()
                .cancel_model(&exchange)
                .unwrap();
        }))
    } else {
        None
    };
    let started = Instant::now();
    let chunks = store
        .execute_model_recovering_authorized(
            &open,
            || authority.get(),
            || {
                admissions.set(admissions.get() + 1);
                Ok(())
            },
            || {
                releases.set(releases.get() + 1);
                if case == "cancel_wait" {
                    sent.send(()).unwrap();
                }
                if case == "authority_wait" {
                    authority.set(false);
                }
            },
        )
        .unwrap();
    if let Some(cancellation) = cancellation {
        cancellation.join().unwrap();
    }
    assert_eq!(
        admissions.get(),
        releases.get(),
        "every attempt releases admission before waiting"
    );
    let attempts:Vec<DeviceAttemptRow> = store.connection.prepare("SELECT attempt_number,state,failure_chunks,response_bytes FROM model_invocation_attempts ORDER BY attempt_number").unwrap().query_map([],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap().collect::<Result<_,_>>().unwrap();
    let expected = if matches!(
        case,
        "rate_limit" | "upstream" | "incomplete" | "incomplete_reset"
    ) {
        2
    } else {
        1
    };
    assert_eq!(
        attempts.len(),
        expected,
        "case {case}: returned chunks {chunks:?}, attempt records {attempts:?}"
    );
    if case == "completed_reset" {
        assert_eq!(
            attempts[0].1, "completed",
            "an abrupt HTTP close after semantic completion and real usage must not pay twice"
        );
        assert!(chunks.iter().all(|chunk| chunk.error.is_none()));
        assert_eq!(
            store
                .execute_model_recovering_authorized(
                    &open,
                    || false,
                    || panic!("complete replay must not call again"),
                    || panic!("no admission")
                )
                .unwrap(),
            chunks
        );
        return;
    }
    if case == "cross_process_cancel" {
        assert_eq!(
            attempts[0].1, "completed",
            "a foreign cancellation scanner cannot settle a live paid request"
        );
        assert!(store.model_cancelled(&open.model_exchange_id.0).unwrap());
        assert!(
            store
                .replay_model(&open.model_exchange_id.0, 1)
                .unwrap()
                .is_empty()
        );
        let key = ActionEnforcementSigningKey::from_bytes([7; 32]).unwrap();
        let statement = store
            .accounting_statement(&open.lease, &key)
            .unwrap()
            .unwrap();
        statement.verify(&key).unwrap();
        assert_eq!(
            statement.receipts[0].tokens,
            Some(5),
            "actual paid response survives cancellation from a separate process"
        );
        return;
    }
    assert_eq!(attempts[0].1, "failed");
    let first: Vec<ModelChunkMessage> =
        serde_json::from_str(attempts[0].2.as_ref().unwrap()).unwrap();
    let diagnostic: serde_json::Value = serde_json::from_slice(
        &STANDARD
            .decode(&first[0].payload.as_ref().unwrap().data_base64)
            .unwrap(),
    )
    .unwrap();
    assert!(!diagnostic.to_string().contains("private"));
    if case == "rejected" {
        assert_eq!(diagnostic["status"], 403);
        assert!(!first[0].error.as_ref().unwrap().retryable);
        assert_eq!(chunks, first);
    } else if case == "cancel_wait" {
        assert!(chunks.is_empty());
        assert!(started.elapsed() < Duration::from_secs(5));
    } else if case == "authority_wait" {
        assert_eq!(
            chunks[0].error.as_ref().unwrap().code,
            ExecutionPortErrorCode::LeaseExpired
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    } else {
        assert!(chunks.last().unwrap().is_final);
        assert!(chunks.iter().all(|chunk| chunk.error.is_none()));
        assert_eq!(attempts[1].1, "completed");
    }
    if case != "cancel_wait" {
        assert_eq!(
            store
                .execute_model_recovering_authorized(
                    &open,
                    || false,
                    || panic!("replay cannot reacquire admission"),
                    || panic!("replay never releases an actual request")
                )
                .unwrap(),
            chunks
        );
    }
    let key = ActionEnforcementSigningKey::from_bytes([7; 32]).unwrap();
    let statement = store
        .accounting_statement(&open.lease, &key)
        .unwrap()
        .unwrap();
    statement.verify(&key).unwrap();
    assert_eq!(
        statement.manifest.len(),
        1,
        "one bounded financial aggregate retains all actual invocation facts"
    );
    if matches!(case, "incomplete" | "incomplete_reset") {
        assert!(attempts[0].3.as_ref().unwrap().starts_with(b"data:"));
        assert_eq!(
            statement.receipts.len(),
            1,
            "observed failed-attempt payment facts survive recovery"
        );
        assert_eq!(
            statement
                .receipts
                .iter()
                .map(|receipt| receipt.tokens.unwrap())
                .sum::<u64>(),
            15
        );
    } else if expected == 2 {
        assert_eq!(
            statement.receipts.len(),
            1,
            "all attempts bind one aggregate"
        );
        assert_eq!(
            statement.receipts[0].tokens, None,
            "known successful usage cannot erase unknown failed-attempt charge"
        );
    }
}

fn success() -> (&'static str, &'static str, &'static str) {
    (
        "200 OK",
        "",
        concat!(
            "data: {\"id\":\"finished-response\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"finished-response\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        ),
    )
}

fn serve(
    listener: &TcpListener,
    config: &Arc<ServerConfig>,
    responses: Vec<(&str, &str, &str)>,
    concurrent_root: Option<&std::path::Path>,
) -> Vec<Vec<u8>> {
    let mut requests = Vec::new();
    for (status, headers, body) in responses {
        let started = Instant::now();
        let socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && started.elapsed() < Duration::from_secs(15) =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("expected actual HTTPS request: {error}"),
            }
        };
        // macOS accepted sockets inherit the listener's nonblocking mode.
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut stream =
            StreamOwned::new(ServerConnection::new(Arc::clone(config)).unwrap(), socket);
        requests.push(read_request(&mut stream));
        if let Some(root) = concurrent_root {
            let store = DeviceProviderStore::open(root).unwrap();
            let (exchange, request): (String, String) = store
                .connection
                .query_row(
                    "SELECT exchange_id,request_open FROM exchanges",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            store.cancel_model(&exchange).unwrap();
            let state: String = store
                .connection
                .query_row("SELECT state FROM model_invocation_attempts", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                state, "invoking",
                "process-local registry absence is not abandonment proof"
            );
            let open: ModelOpenMessage = serde_json::from_str(&request).unwrap();
            let key = ActionEnforcementSigningKey::from_bytes([7; 32]).unwrap();
            assert!(
                store
                    .accounting_statement(&open.lease, &key)
                    .unwrap()
                    .is_none(),
                "a still-running model cannot have a closed charge ledger"
            );
        }
        let declared_length = body.len()
            + if headers.contains("X-Test-Truncated-Length") {
                100
            } else {
                0
            };
        write!(stream,"HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nX-Request-Id: provider-request-{}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",requests.len(),declared_length).unwrap();
        stream.flush().unwrap();
    }
    requests
}

fn read_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
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
                return bytes;
            }
        }
    }
}

fn idempotency_key(request: &[u8]) -> String {
    std::str::from_utf8(request)
        .unwrap()
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("idempotency-key")
                .then(|| value.trim().to_owned())
        })
        .unwrap()
}
