use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_code_mode_protocol::CodeModeSessionCellExecutionLimits;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::NoopCodeModeSessionDelegate;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::WaitRequest;
use codex_code_mode_protocol::host::CapabilitySet;
use codex_code_mode_protocol::host::HostHello;
use codex_code_mode_protocol::host::HostResponse;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::ProtocolVersion;
use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireCellId;
use codex_code_mode_protocol::host::WireResult;
use codex_code_mode_protocol::host::WireRuntimeResponse;
use codex_code_mode_protocol::host::WireWaitOutcome;
use serde_json::json;

use super::Connection;
use super::RemoteSession;

struct FakeHost {
    root: PathBuf,
    program: PathBuf,
}
impl FakeHost {
    fn new(mode: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("wwc-fcr01-{}-{mode}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let program = root.join("owned-stdio-host");
        let python = std::process::Command::new("python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
            .unwrap();
        assert!(python.status.success());
        let python = String::from_utf8(python.stdout).unwrap();
        let session_id = SessionId::new("session-1".to_string()).unwrap();
        let cell_id = WireCellId::new("1");
        // Every reply template is produced by the canonical Rust protocol serializer.
        let templates = json!({
            "hello": HostToClient::HostHello(HostHello::new(ProtocolVersion::V1, CapabilitySet::empty())),
            "open": HostToClient::Response { id: RequestId::new(1), result: WireResult::Ok { value: HostResponse::SessionReady { session_id: session_id.clone() } } },
            "start": HostToClient::Response { id: RequestId::new(2), result: WireResult::Ok { value: HostResponse::ExecutionStarted { cell_id: cell_id.clone() } } },
            "initial": HostToClient::InitialResponse { id: RequestId::new(2), result: WireResult::Ok { value: WireRuntimeResponse::Yielded { cell_id: cell_id.clone(), content_items: Vec::new() } } },
            "wait": HostToClient::Response { id: RequestId::new(3), result: WireResult::Ok { value: HostResponse::WaitCompleted { outcome: WireWaitOutcome::LiveCell(WireRuntimeResponse::Yielded { cell_id: cell_id.clone(), content_items: Vec::new() }) } } },
            "terminate": HostToClient::Response { id: RequestId::new(3), result: WireResult::Ok { value: HostResponse::WaitCompleted { outcome: WireWaitOutcome::LiveCell(WireRuntimeResponse::Result { cell_id: cell_id.clone(), content_items: Vec::new(), error_text: None }) } } },
            "close": HostToClient::Response { id: RequestId::new(4), result: WireResult::Ok { value: HostResponse::SessionClosed { session_id: session_id.clone() } } },
            "closed": HostToClient::CellClosed { session_id, cell_id },
            "cancel": HostToClient::Response { id: RequestId::new(1), result: WireResult::<HostResponse>::Err { message: "owned fake host acknowledged cancellation".to_string() } },
        });
        let script = format!(
            "#!{}\nimport json\nMODE={}\nROOT={}\nTEMPLATES=json.loads({})\n{}",
            python.trim(),
            serde_json::to_string(mode).unwrap(),
            serde_json::to_string(&root.to_string_lossy()).unwrap(),
            serde_json::to_string(&serde_json::to_string(&templates).unwrap()).unwrap(),
            FAKE_HOST_BODY
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { root, program }
    }
    async fn observed(&self, marker: &str) {
        let start = std::time::Instant::now();
        while !self.root.join(marker).is_file() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "fake host did not observe {marker}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    fn cancellations(&self) -> usize {
        std::fs::read_to_string(self.root.join("operations.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "operation/cancel")
            .count()
    }
    async fn assert_reaped(&self) {
        let pid = std::fs::read_to_string(self.root.join("pid")).unwrap();
        let start = std::time::Instant::now();
        loop {
            let status = std::process::Command::new("/bin/kill")
                .args(["-0", pid.trim()])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            if !status.success() {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "owned host not reaped within bound"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}
impl Drop for FakeHost {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// Fault-injection peer only. All client operation/timeout/cancellation behavior is actual Rust.
const FAKE_HOST_BODY: &str = r#"
import copy, json, os, pathlib, struct, sys
root = pathlib.Path(ROOT)
(root / 'pid').write_text(str(os.getpid()))
blocked_once = False
def read_exact(length):
    result = b''
    while len(result) < length:
        chunk = sys.stdin.buffer.read(length - len(result))
        if not chunk: return None
        result += chunk
    return result
def send(template, request_id=None, session_id=None):
    message = copy.deepcopy(TEMPLATES[template])
    if request_id is not None: message['id'] = request_id
    if session_id is not None:
        if 'sessionId' in message: message['sessionId'] = session_id
        if 'result' in message and 'sessionId' in message['result'].get('value', {}): message['result']['value']['sessionId'] = session_id
    payload = json.dumps(message, separators=(',', ':')).encode()
    sys.stdout.buffer.write(struct.pack('<I', len(payload)) + payload)
    sys.stdout.buffer.flush()
while True:
    prefix = read_exact(4)
    if prefix is None: break
    payload = read_exact(struct.unpack('<I', prefix)[0])
    if payload is None: break
    message = json.loads(payload)
    kind = message['type']
    with (root / 'operations.log').open('a') as log: log.write(kind + '\n')
    if kind == 'connection/hello':
        send('hello'); (root / 'handshake').touch(); continue
    if kind == 'operation/cancel':
        (root / 'cancel').touch(); send('cancel', message['id']); continue
    if kind != 'operation/request': continue
    request = message['request']; method = request['method']; request_id = message['id']; session_id = request['sessionId']
    block = {'open':'session/open', 'start':'session/execute', 'close':'session/shutdown', 'wait':'session/wait', 'terminate':'session/terminate', 'opening_outer_cancel':'session/open'}.get(MODE)
    if method == block and not blocked_once:
        blocked_once = True; (root / ('withheld-' + MODE)).touch(); continue
    if method == 'session/open': send('open', request_id, session_id)
    elif method == 'session/execute':
        send('start', request_id)
        if MODE == 'initial' and not blocked_once:
            blocked_once = True; (root / 'withheld-initial').touch()
        else: send('initial', request_id)
    elif method == 'session/wait': send('wait', request_id)
    elif method == 'session/terminate': send('terminate', request_id); send('closed', session_id=session_id)
    elif method == 'session/shutdown': send('close', request_id, session_id)
"#;

fn remote() -> RemoteSession {
    RemoteSession {
        id: SessionId::new("session-1".to_string()).unwrap(),
        generation: 1,
    }
}
fn request() -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "synthetic-fcr01".to_string(),
        enabled_tools: Vec::new(),
        source: "await new Promise(() => {});".to_string(),
        yield_time_ms: Some(1),
        max_output_tokens: None,
    }
}
async fn advance_transport_interval() {
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(70)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    tokio::time::resume();
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "mechanism audit: FC-R01 known transport-deadline defect"]
async fn mechanism_fcr01_stdio_boundaries_have_transport_deadlines() {
    let mut missing = Vec::new();
    for mode in ["open", "start", "initial", "close", "wait", "terminate"] {
        let fake = FakeHost::new(mode);
        let connection = Arc::new(
            Connection::spawn(&fake.program)
                .await
                .map_err(|err| err.to_string())
                .unwrap(),
        );
        let cleanup = if mode != "open" {
            Some(
                connection
                    .open_session(
                        remote(),
                        Arc::new(NoopCodeModeSessionDelegate),
                        CodeModeSessionCellExecutionLimits::default(),
                    )
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let started = if matches!(mode, "initial" | "wait" | "terminate") {
            Some(connection.execute(remote(), request()).await.unwrap())
        } else {
            None
        };
        let cell_id = started.as_ref().map(|started| started.cell_id.clone());
        // Consume ordinary initial yields for controls; preserve the stalled initial receiver.
        let initial = if mode == "initial" {
            started
        } else {
            if let Some(started) = started {
                assert!(matches!(
                    started.initial_response().await.unwrap(),
                    RuntimeResponse::Yielded { .. }
                ));
            }
            None
        };
        let c = Arc::clone(&connection);
        let task = match mode {
            "open" => tokio::spawn(async move {
                c.open_session(
                    remote(),
                    Arc::new(NoopCodeModeSessionDelegate),
                    CodeModeSessionCellExecutionLimits::default(),
                )
                .await
                .map(|_| ())
            }),
            "start" => {
                tokio::spawn(async move { c.execute(remote(), request()).await.map(|_| ()) })
            }
            "initial" => {
                tokio::spawn(async move { initial.unwrap().initial_response().await.map(|_| ()) })
            }
            "close" => tokio::spawn(async move { c.shutdown_session(remote()).await }),
            "wait" => tokio::spawn(async move {
                c.wait(
                    remote(),
                    WaitRequest {
                        cell_id: cell_id.unwrap(),
                        yield_time_ms: 1,
                    },
                )
                .await
                .map(|_| ())
            }),
            "terminate" => {
                tokio::spawn(
                    async move { c.terminate(remote(), cell_id.unwrap()).await.map(|_| ()) },
                )
            }
            _ => unreachable!(),
        };
        fake.observed(&format!("withheld-{mode}")).await;
        advance_transport_interval().await;
        let finished_after_70s = task.is_finished();
        let alive_after_70s = connection.is_alive();
        let result = if finished_after_70s {
            format!("{:?}", task.await.unwrap())
        } else {
            task.abort();
            let _ = task.await;
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
            "Pending; outer caller aborted".to_string()
        };
        // Give the actual driver cancellation watcher and pipe writer real time to run.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let cancel_frames = fake.cancellations();
        connection.cancellation.cancel();
        if let Some(cleanup) = cleanup {
            tokio::time::timeout(Duration::from_secs(2), cleanup.wait())
                .await
                .unwrap();
        }
        fake.assert_reaped().await;
        let row = json!({"id":"FC-R01","phase":mode,"legal_handshake":true,"virtual_elapsed_s":70,"future_finished":finished_after_70s,"connection_alive_before_fixture_cleanup":alive_after_70s,"outer_cancel_frames":cancel_frames,"result":result,"owned_process_reaped":true,"source":"actual Connection::spawn and stdio driver"});
        eprintln!("MECHANISM_RECEIPT {row}");
        if matches!(mode, "wait" | "terminate") {
            assert!(
                finished_after_70s && !alive_after_70s,
                "actual transport-deadline positive control failed"
            );
        } else if !finished_after_70s {
            missing.push(mode);
        }
    }
    assert!(
        missing.is_empty(),
        "stdio boundaries exceeded the actual 60-second transport budget: {missing:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "mechanism audit: FC-R01 known Opening cancellation defect"]
async fn mechanism_fcr01_opening_outer_cancel_retires_shutdown() {
    use super::super::OwnedCodeModeHost;
    use super::super::ProcessOwnedCodeModeSession;
    let fake = FakeHost::new("opening_outer_cancel");
    let host = Arc::new(OwnedCodeModeHost::new(fake.program.clone()));
    let session = Arc::new(ProcessOwnedCodeModeSession::with_host(
        Arc::new(NoopCodeModeSessionDelegate),
        Arc::clone(&host),
        CodeModeSessionCellExecutionLimits::default(),
    ));
    let executing = Arc::clone(&session);
    let execute_task = tokio::spawn(async move { executing.execute(request()).await.map(|_| ()) });
    fake.observed("withheld-opening_outer_cancel").await;
    let shutting_down = Arc::clone(&session);
    let shutdown_task = tokio::spawn(async move { shutting_down.shutdown().await });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    advance_transport_interval().await;
    let opening_pending = !execute_task.is_finished();
    let shutdown_pending = !shutdown_task.is_finished();
    execute_task.abort();
    let _ = execute_task.await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let shutdown_after_outer_cancel = !shutdown_task.is_finished();
    let cancel_frames = fake.cancellations();
    let connection = host.live_connection().unwrap();
    connection.cancellation.cancel();
    let shutdown_result = tokio::time::timeout(Duration::from_secs(2), shutdown_task)
        .await
        .unwrap()
        .unwrap();
    fake.assert_reaped().await;
    let row = json!({"id":"FC-R01","phase":"Opening + actual SessionInner::drive_shutdown","virtual_elapsed_s":70,"open_pending":opening_pending,"shutdown_pending":shutdown_pending,"shutdown_pending_after_outer_execute_abort":shutdown_after_outer_cancel,"cancel_frames_after_outer_abort":cancel_frames,"shutdown_result_after_owned_connection_cancel":format!("{shutdown_result:?}"),"owned_process_reaped":true});
    eprintln!("MECHANISM_RECEIPT {row}");
    assert!(
        !shutdown_after_outer_cancel,
        "outer execute cancellation must retire Opening and pending shutdown"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn mechanism_fcr01_stdio_long_yield_request_is_accepted() {
    let fake = FakeHost::new("long_yield");
    let connection = Connection::spawn(&fake.program)
        .await
        .map_err(|err| err.to_string())
        .unwrap();
    let _failure_cleanup = connection
        .open_session(
            remote(),
            Arc::new(NoopCodeModeSessionDelegate),
            CodeModeSessionCellExecutionLimits::default(),
        )
        .await
        .unwrap();
    let started = connection.execute(remote(), request()).await.unwrap();
    let cell_id = started.cell_id.clone();
    assert!(matches!(
        started.initial_response().await.unwrap(),
        RuntimeResponse::Yielded { .. }
    ));
    let outcome = connection
        .wait(
            remote(),
            WaitRequest {
                cell_id,
                yield_time_ms: 600_001,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        codex_code_mode_protocol::WaitOutcome::LiveCell(RuntimeResponse::Yielded { .. })
    ));
    assert!(connection.is_alive());
    connection.shutdown_session(remote()).await.unwrap();
    // SessionCleanup is a failed-connection drain barrier, not a successful-close barrier.
    connection.cancellation.cancel();
    fake.assert_reaped().await;
    eprintln!(
        "MECHANISM_RECEIPT {}",
        json!({"id":"FC-R01","phase":"normal wire yield control","requested_yield_ms":600_001,"yield_accepted":true,"connection_remained_alive":true,"owned_process_reaped":true,"limitation":"fake peer returns a legal yield; it does not execute JavaScript"})
    );
}
