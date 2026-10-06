//! Exercise the embedded Core and the built product host through the `ModelPort` boundary.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};
use winwincode_execution_port::agent_config::{AgentProfileSettings, resolve_agent_session_config};
use winwincode_kernel::{
    EventPoll, Kernel, KernelActionAuthorization, KernelActionGate, KernelActionPayload,
    KernelActionRequest, KernelFailure, KernelOptions, ModelPort, ModelPortFailure,
    ModelPortRequest, ModelPortStream, SessionOptions, TurnSubmissionOptions,
};

const EXEC_SOURCE: &str = r"
const echo = ALL_TOOLS.find(tool => tool.name.endsWith('fixture__echo'));
if (!echo) throw new Error('fixture missing from authorized catalog');
const definition = toolDefinition(echo.name);
if (definition.input_schema.required[0] !== 'value') throw new Error('input schema missing');
text('catalog-and-definition-ok');
text(await tools[echo.name]({value:'mcp-through-core'}));
const shell = ALL_TOOLS.find(tool => tool.name.endsWith('exec_command'));
if (!shell) throw new Error('exec_command missing from authorized catalog: ' + JSON.stringify(ALL_TOOLS));
text(await tools[shell.name]({cmd:'printf shell-through-core'}));
if (typeof fetch !== 'undefined' || typeof process !== 'undefined' || typeof require !== 'undefined')
  throw new Error('ambient host access exposed');
text('controlled-host-interface-ok');
text('icu-data-ok:' + new Intl.DateTimeFormat('en-US', {timeZone:'UTC'})
  .format(new Date('2026-01-01T00:00:00Z')));
try { await import('node:fs'); } catch { text('host-import-rejected'); }
try { toolDefinition('unauthorized_tool'); } catch { text('unknown-definition-rejected'); }
text('before-yield');
await yield_control();
await new Promise(resolve => setTimeout(resolve, 250));
text('after-yield');
";

const CANCEL_SOURCE: &str = r"
text('before-cancel');
await yield_control();
await new Promise(resolve => setTimeout(resolve, 60000));
text('should-never-complete');
";

#[derive(Debug)]
struct ScriptedModel {
    requests: Mutex<Vec<Value>>,
    source: &'static str,
    cancel_boundary: Option<Arc<tokio::sync::Notify>>,
    cancelled_cell: Mutex<Option<String>>,
}

fn call_item(name: &str, call_id: &str, arguments: &Value) -> Value {
    if name == "exec" {
        json!({"type":"custom_tool_call", "call_id":call_id, "name":name, "input":arguments})
    } else {
        json!({"type":"function_call", "call_id":call_id, "name":name,
            "arguments":arguments.to_string()})
    }
}

impl ModelPort for ScriptedModel {
    fn stream(
        &self,
        request: ModelPortRequest,
    ) -> BoxFuture<'static, Result<ModelPortStream, ModelPortFailure>> {
        let payload: Value = serde_json::from_str(&request.payload_json).unwrap();
        let mut requests = self.requests.lock().unwrap();
        let index = requests.len();
        let input = payload["request"]["input"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|item| {
                matches!(
                    item["type"].as_str(),
                    Some("custom_tool_call_output" | "function_call_output")
                )
            })
            .map(Value::to_string)
            .unwrap_or_default();
        requests.push(payload);
        if index == 1
            && let Some(boundary) = &self.cancel_boundary
        {
            let (_, rest) = input
                .split_once("Script running with cell ID ")
                .expect("live cell before cancellation");
            *self.cancelled_cell.lock().unwrap() = Some(
                rest.split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                    .next()
                    .unwrap()
                    .to_owned(),
            );
            boundary.notify_one();
            return Box::pin(async { Ok(Box::pin(futures::stream::pending()) as ModelPortStream) });
        }
        let item = if index == 0 {
            call_item("exec", "exec-first", &json!(self.source))
        } else if index == 2 && self.cancel_boundary.is_some() {
            call_item(
                "wait",
                "wait-cancelled",
                &json!({"cell_id":self.cancelled_cell.lock().unwrap().as_ref().unwrap(),"yield_time_ms":1000}),
            )
        } else if let Some((_, rest)) = input.rsplit_once("Script running with cell ID ") {
            let cell_id = rest
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                .next()
                .unwrap();
            call_item(
                "wait",
                &format!("wait-{index}"),
                &json!({"cell_id":cell_id,"yield_time_ms":1000}),
            )
        } else {
            json!({"type":"message","role":"assistant","phase":"final_answer",
                "content":[{"type":"output_text","text":"fixture completed"}]})
        };
        assert!(index < 6, "cell did not finish: {input}");
        let frames = [
            json!({"type":"created"}).to_string(),
            json!({"type":"output_item_done","item":item}).to_string(),
            json!({"type":"completed","responseId":format!("response-{index}"),
                "endTurn":item["type"] == "message"})
            .to_string(),
        ];
        Box::pin(async move {
            Ok(Box::pin(futures::stream::iter(frames.into_iter().map(Ok))) as ModelPortStream)
        })
    }
}

#[derive(Default)]
struct RecordingGate(Mutex<Vec<KernelActionRequest>>);

impl KernelActionGate for RecordingGate {
    fn authorize(
        &self,
        request: KernelActionRequest,
    ) -> BoxFuture<'static, Result<KernelActionAuthorization, KernelFailure>> {
        let authorization = KernelActionAuthorization::new(request.operation_id.clone(), None);
        self.0.lock().unwrap().push(request);
        Box::pin(async move { Ok(authorization) })
    }

    fn revalidate(
        &self,
        request: KernelActionRequest,
        authorization: KernelActionAuthorization,
    ) -> BoxFuture<'static, Result<(), KernelFailure>> {
        Box::pin(async move {
            if request.operation_id == authorization.request_binding() {
                Ok(())
            } else {
                Err(KernelFailure::action_rejected())
            }
        })
    }
}

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "wwc-code-mode-kernel-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("workspace")).unwrap();
        std::fs::create_dir(root.join("home")).unwrap();
        let product_helper = std::env::var_os("WINWINCODE_CODE_MODE_TEST_HELPER").map_or_else(
            || {
                std::env::current_exe()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("winwincode-kernel-helper")
            },
            PathBuf::from,
        );
        assert!(
            product_helper.is_file(),
            "build the product helper before this test"
        );
        std::fs::copy(product_helper, root.join("helper")).unwrap();
        #[cfg(target_os = "linux")]
        std::fs::hard_link(root.join("helper"), root.join("codex-linux-sandbox")).unwrap();
        let mcp = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/code-mode-mcp.mjs");
        std::fs::write(root.join("home/config.toml"), format!(
            "[features]\ncode_mode_only = true\n[mcp_servers.fixture]\ncommand = \"node\"\nargs = [{}]\nrequired = true\n",
            serde_json::to_string(&mcp.canonicalize().unwrap()).unwrap()
        )).unwrap();
        Self(root)
    }

    fn kernel_options(&self) -> KernelOptions {
        let mut options = KernelOptions::new(self.0.join("home"), self.0.join("helper"));
        // Product composition supplies this alias so older bubblewrap can
        // re-execute the sandbox helper through argv0.
        options.linux_sandbox_executable =
            cfg!(target_os = "linux").then(|| self.0.join("codex-linux-sandbox"));
        options
    }

    fn session_options(&self) -> SessionOptions {
        let worker = serde_json::from_value(json!("wrk_code_mode_fixture")).unwrap();
        let platform = format!(
            "{}-{}",
            std::env::consts::ARCH,
            if cfg!(target_os = "macos") {
                "apple-darwin"
            } else {
                "unknown-linux-gnu"
            }
        );
        let capabilities = serde_json::from_value(json!({
            "capabilityDigest":format!("sha256:{}", "a".repeat(64)),
            "features":["sandbox"],"maxConcurrentJobs":1,"platform":platform
        }))
        .unwrap();
        SessionOptions {
            cwd: self.0.join("workspace"),
            provider: "fixture-provider".into(),
            model: "gpt-5.6-sol".into(),
            role_policy: None,
            agent_config: resolve_agent_session_config(
                &worker,
                &capabilities,
                "codex-chat",
                AgentProfileSettings {
                    fusion: None,
                    jev_context: None,
                    jev_judge: None,
                    provider: "fixture-provider".into(),
                    model: "gpt-5.6-sol".into(),
                    reasoning: "provider_default".into(),
                    tools: vec!["worker:sandbox".into()],
                    sandbox: "read-only".into(),
                    instructions: None,
                },
            )
            .unwrap(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_native_test(test: impl FnOnce(tokio::runtime::Runtime) + Send + 'static) {
    // Core runs on the block_on caller as well as the runtime's worker threads.
    std::thread::Builder::new()
        .name("native-code-mode-test".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap();
            test(runtime);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn native_host_discovers_calls_and_waits_through_the_real_kernel() {
    run_native_test(|runtime| {
        runtime.block_on(async {
            let fixture = Fixture::new();
            let model = Arc::new(ScriptedModel {
                requests: Mutex::new(Vec::new()),
                source: EXEC_SOURCE,
                cancel_boundary: None,
                cancelled_cell: Mutex::new(None),
            });
            let gate = Arc::new(RecordingGate::default());
            let kernel =
                Kernel::new(fixture.kernel_options(), model.clone(), gate.clone()).unwrap();
            let session = kernel
                .create_session(fixture.session_options())
                .await
                .unwrap();
            kernel
                .submit_turn(
                    &session.session_id,
                    "Exercise native Code Mode".into(),
                    TurnSubmissionOptions::default(),
                )
                .await
                .unwrap();
            wait_for_kind(&kernel, &session.session_id, "turn_complete").await;
            kernel.shutdown().await.unwrap();
            let requests = model.requests.lock().unwrap();
            let outputs = requests
                .iter()
                .flat_map(|request| request["request"]["input"].as_array().unwrap())
                .filter(|item| {
                    matches!(
                        item["type"].as_str(),
                        Some("custom_tool_call_output" | "function_call_output")
                    )
                })
                .map(|item| item["output"].to_string())
                .collect::<Vec<_>>()
                .join("\n");
            for expected in [
                "catalog-and-definition-ok",
                "mcp-through-core",
                "shell-through-core",
                "unknown-definition-rejected",
                "host-import-rejected",
                "controlled-host-interface-ok",
                "icu-data-ok:1/1/2026",
                "Script running with cell ID",
                "before-yield",
                "after-yield",
                "Script completed",
            ] {
                assert!(outputs.contains(expected), "missing {expected}: {outputs}");
            }
            let admitted = gate.0.lock().unwrap();
            assert!(
                admitted
                    .iter()
                    .any(|request| matches!(request.payload, KernelActionPayload::Shell { .. }))
            );
            assert!(
                admitted
                    .iter()
                    .any(|request| request.tool_name.contains("fixture")
                        || request.namespace.as_deref() == Some("mcp__fixture"))
            );
        });
    });
}

#[test]
fn user_interrupt_closes_the_native_cell_before_the_next_turn() {
    run_native_test(|runtime| {
        runtime.block_on(async {
            let fixture = Fixture::new();
            let boundary = Arc::new(tokio::sync::Notify::new());
            let model = Arc::new(ScriptedModel {
                requests: Mutex::new(Vec::new()),
                source: CANCEL_SOURCE,
                cancel_boundary: Some(boundary.clone()),
                cancelled_cell: Mutex::new(None),
            });
            let kernel = Kernel::new(
                fixture.kernel_options(),
                model.clone(),
                Arc::new(RecordingGate::default()),
            )
            .unwrap();
            let session = kernel
                .create_session(fixture.session_options())
                .await
                .unwrap();
            kernel
                .submit_turn(
                    &session.session_id,
                    "Start an interruptible cell".into(),
                    TurnSubmissionOptions::default(),
                )
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(30), boundary.notified())
                .await
                .expect("cell yielded");
            kernel.interrupt(&session.session_id).await.unwrap();
            wait_for_kind(&kernel, &session.session_id, "turn_aborted").await;
            kernel
                .submit_turn(
                    &session.session_id,
                    "Check the interrupted cell".into(),
                    TurnSubmissionOptions::default(),
                )
                .await
                .unwrap();
            wait_for_kind(&kernel, &session.session_id, "turn_complete").await;
            kernel.shutdown().await.unwrap();
            let requests = model.requests.lock().unwrap();
            let input = requests.last().unwrap()["request"]["input"]
                .as_array()
                .unwrap();
            let last_input = input
                .iter()
                .find(|item| {
                    item["type"] == "function_call_output" && item["call_id"] == "wait-cancelled"
                })
                .expect("wait result after cancellation")["output"]
                .to_string();
            assert!(
                last_input.contains("not found"),
                "cancelled cell remained live: {last_input}"
            );
            assert!(!last_input.contains("should-never-complete"));
        });
    });
}

async fn wait_for_kind(kernel: &Kernel, session_id: &str, kind: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match kernel.next_event(session_id, None).await.unwrap() {
                EventPoll::Event(event) if event.kind == kind => break,
                EventPoll::Event(event) => {
                    assert_ne!(event.kind, "error", "{}", event.payload_json);
                }
                EventPoll::Closed => panic!("session closed before {kind}"),
                EventPoll::Timeout => {}
            }
        }
    })
    .await
    .expect("expected Core event");
}
