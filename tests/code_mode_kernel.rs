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
text(await tools[shell.name]({cmd:'printf shell-through-core', login:false}));
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
text(await tools[shell.name]({cmd:'printf shell-after-yield', login:false}));
text(await tools[echo.name]({value:'mcp-after-yield'}));
const mediaTool = ALL_TOOLS.find(tool => tool.name.endsWith('fixture__media'));
const media = await tools[mediaTool.name]({});
image(media.content.find(item => item.type === 'image'));
audio(media.content.find(item => item.type === 'audio'));
text(await tools.list_mcp_resources({}));
text(await tools.list_mcp_resource_templates({server:'fixture'}));
text(await tools.read_mcp_resource({server:'fixture', uri:'fixture://readme'}));
const viewed = await tools.view_image({path:'image.png'});
image(viewed.image_url);
const terminal = await tools.exec_command({cmd:'bash --noprofile --norc', login:false, tty:true, yield_time_ms:250});
text(await tools.write_stdin({session_id:terminal.session_id, chars:'printf terminal-input-through-core; exit\n', yield_time_ms:10000}));
";

const CANCEL_SOURCE: &str = r"
text('before-cancel');
await yield_control();
await new Promise(resolve => setTimeout(resolve, 60000));
text('should-never-complete');
";

const QUESTION_SOURCE: &str = r"
text('question-before');
const response = await tools.request_user_input({questions:[{
  id:'decision', header:'Decision', question:'Continue the fixture?', options:[
    {label:'Proceed', description:'Continue the cell.'},
    {label:'Revise', description:'Change the approach.'}
  ]
}]});
text('question-answer:' + JSON.stringify(response));
await yield_control();
text('question-after-yield');
";

const HANDOFF_SOURCE: &str = r#"
await tools.submit_change_batch('{"acceptanceCriteriaIds":["fixture"],"disposition":"final","patch":"fixture-patch","schemaVersion":1,"validationProfile":"fixture"}');
await tools.exec_command({cmd:'printf forbidden-after-handoff'});
"#;

#[derive(Debug)]
struct ScriptedModel {
    requests: Mutex<Vec<Value>>,
    source: &'static str,
    exec_count: usize,
    cancel_boundary: Option<(Arc<tokio::sync::Notify>, CancelBoundary)>,
    cancelled_cell: Mutex<Option<String>>,
    direct_probe: bool,
}

#[derive(Clone, Copy, Debug)]
enum CancelBoundary {
    PendingModel,
    ActiveWait,
}

impl ScriptedModel {
    fn wait_yield_time(&self, step: usize) -> u64 {
        if step == 1 && matches!(self.cancel_boundary, Some((_, CancelBoundary::ActiveWait))) {
            30_000
        } else {
            1000
        }
    }
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
        assert_eq!(
            payload["request"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"]
                    .as_str()
                    .or_else(|| tool["type"].as_str())
                    .unwrap())
                .collect::<Vec<_>>(),
            ["exec", "wait"],
        );
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
        let step = index.saturating_sub(usize::from(self.direct_probe));
        if step == 1
            && let Some((boundary, phase)) = &self.cancel_boundary
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
            if matches!(phase, CancelBoundary::PendingModel) {
                return Box::pin(async {
                    Ok(Box::pin(futures::stream::pending()) as ModelPortStream)
                });
            }
        }
        let item = if self.direct_probe && index == 0 {
            call_item(
                "exec_command",
                "direct-probe",
                &json!({"cmd":"printf forbidden-direct"}),
            )
        } else if step == 0 {
            call_item("exec", "exec-first", &json!(self.source))
        } else if step == 2 && self.cancel_boundary.is_some() {
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
            let yield_time_ms = self.wait_yield_time(step);
            call_item(
                "wait",
                &format!("wait-{index}"),
                &json!({"cell_id":cell_id,"yield_time_ms":yield_time_ms}),
            )
        } else if requests.last().unwrap()["request"]["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "custom_tool_call" && item["name"] == "exec")
            .count()
            < self.exec_count
        {
            call_item("exec", &format!("exec-{index}"), &json!(self.source))
        } else {
            json!({"type":"message","role":"assistant","phase":"final_answer",
                "content":[{"type":"output_text","text":"fixture completed"}]})
        };
        assert!(index < 20, "cell did not finish: {input}");
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
        let mcp_arg = serde_json::to_string(&mcp.canonicalize().unwrap()).unwrap();
        let oracle_arg = serde_json::to_string(&root.join("echo-calls.jsonl")).unwrap();
        let catalog: Value = serde_json::from_str(include_str!(
            "../third_party/codex/codex-rs/models-manager/models.json"
        ))
        .unwrap();
        let mut model = catalog["models"][0].clone();
        model["slug"] = json!("gpt-5.6-sol");
        // The scripted transport accepts all three media kinds. Declare that
        // capability so Core preserves the fixture's MCP audio content.
        model["input_modalities"] = json!(["text", "image", "audio"]);
        std::fs::write(
            root.join("home/models.json"),
            serde_json::to_vec(&json!({"models":[model]})).unwrap(),
        )
        .unwrap();
        std::fs::write(root.join("home/config.toml"), format!(
            "model_catalog_json = \"models.json\"\n[features]\ncode_mode_only = false\n[mcp_servers.fixture]\ncommand = \"node\"\nargs = [{mcp_arg}, {oracle_arg}]\nrequired = true\n[mcp_servers.fixture_other]\ncommand = \"node\"\nargs = [{mcp_arg}]\nrequired = true\n"
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

    fn configure_cancellation_mcp(&self) {
        let mcp = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/code-mode-cancellation-mcp.mjs")
            .canonicalize()
            .unwrap();
        std::fs::write(
            self.0.join("home/config.toml"),
            format!(
                "model_catalog_json = \"models.json\"\n[mcp_servers.fixture]\ncommand = \"node\"\nargs = [{}, {}]\nrequired = true\n",
                serde_json::to_string(&mcp).unwrap(),
                serde_json::to_string(&self.0).unwrap(),
            ),
        )
        .unwrap();
    }

    fn assert_no_mcp_phase(&self, phase: &str) {
        let oracle = std::fs::read_to_string(self.0.join("calls.jsonl")).unwrap();
        assert!(
            !oracle.lines().any(|line| {
                let observation: Value = serde_json::from_str(line).unwrap();
                observation["phase"] == phase
            }),
            "cancelled cell must not issue MCP phase {phase}: {oracle}"
        );
    }

    fn write_image(&self) {
        std::fs::write(
            self.0.join("workspace/image.png"),
            [
                137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0,
                1, 8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248,
                207, 192, 240, 31, 0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68,
                174, 66, 96, 130,
            ],
        )
        .unwrap();
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

fn assert_io_authorizations(admitted: &[KernelActionRequest]) {
    assert!(admitted.iter().any(|request| matches!(&request.payload, KernelActionPayload::FileRead { path } if path.ends_with("image.png"))));
    for method in [
        "resources/list",
        "resources/templates/list",
        "resources/read",
    ] {
        assert!(admitted.iter().any(|request| matches!(&request.payload, KernelActionPayload::McpResource { server, method: actual, .. } if server == "fixture" && actual == method)));
    }
    assert!(admitted.iter().any(|request| matches!(&request.payload, KernelActionPayload::McpResource { server, method, .. } if server == "fixture_other" && method == "resources/list")));
    assert!(admitted.iter().any(|request| matches!(&request.payload, KernelActionPayload::ProcessInteraction { input, origin_call_id, .. } if !input.is_empty() && !origin_call_id.is_empty())));
}

async fn assert_closed_tool_facts(kernel: &Kernel, session_id: &str) {
    let before_close = kernel
        .tool_runtime_events(session_id, 0, 200)
        .await
        .unwrap();
    kernel.close_session(session_id).await.unwrap();
    let after_close = kernel
        .tool_runtime_events(session_id, 0, 200)
        .await
        .unwrap();
    assert_eq!(
        after_close, before_close,
        "closed Core receipts remain readable"
    );
}

async fn assert_durable_tool_facts(kernel: &Kernel, session_id: &str) {
    let facts = kernel
        .tool_runtime_events(session_id, 0, 200)
        .await
        .unwrap();
    assert!(
        facts
            .windows(2)
            .all(|events| events[0].source_sequence < events[1].source_sequence)
    );
    let values: Vec<Value> = facts
        .iter()
        .map(|event| serde_json::from_str(&event.fact_json).unwrap())
        .collect();
    for kind in ["request", "cell", "wait"] {
        assert!(
            values.iter().any(|value| value["kind"] == kind),
            "missing {kind}"
        );
    }
    assert!(
        values
            .iter()
            .filter(|value| value["kind"] == "request")
            .any(|value| value["fact"]["request"]["source"] == "code_mode")
    );
    assert!(
        facts
            .iter()
            .all(|event| !event.fact_json.contains("shell-through-core"))
    );
    let last = facts.last().unwrap().source_sequence;
    assert!(
        kernel
            .tool_runtime_events(session_id, last, 1)
            .await
            .unwrap()
            .is_empty()
    );
}

#[test]
fn native_host_discovers_calls_and_waits_through_the_real_kernel() {
    run_native_test(|runtime| {
        runtime.block_on(async {
            let fixture = Fixture::new();
            fixture.write_image();
            let model = Arc::new(ScriptedModel {
                requests: Mutex::new(Vec::new()),
                source: EXEC_SOURCE,
                exec_count: 1,
                cancel_boundary: None,
                cancelled_cell: Mutex::new(None),
                direct_probe: false,
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
            assert_durable_tool_facts(&kernel, &session.session_id).await;
            assert_closed_tool_facts(&kernel, &session.session_id).await;
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
                "shell-after-yield",
                "mcp-after-yield",
                "resource-through-core",
                "terminal-input-through-core",
                "Script completed",
            ] {
                assert!(outputs.contains(expected), "missing {expected}: {outputs}");
            }
            let admitted = gate.0.lock().unwrap();
            assert_io_authorizations(&admitted);
            assert!(
                admitted
                    .iter()
                    .filter(|request| matches!(request.tool_name.as_str(), "exec" | "wait"))
                    .all(|request| matches!(
                        request.payload,
                        KernelActionPayload::CoreControl { .. }
                    ))
            );
            let history = requests
                .iter()
                .flat_map(|request| request["request"]["input"].as_array().unwrap())
                .filter_map(|item| item["output"].as_array())
                .flatten()
                .collect::<Vec<_>>();
            for kind in ["input_image", "input_audio"] {
                assert!(
                    history.iter().any(|block| block["type"] == kind),
                    "missing {kind} content block"
                );
            }
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
fn native_cell_hands_control_to_the_host_through_common_dispatch() {
    run_native_test(|runtime| {
        runtime.block_on(async {
            let fixture = Fixture::new();
            let model = Arc::new(ScriptedModel {
                requests: Mutex::new(Vec::new()),
                source: HANDOFF_SOURCE,
                exec_count: 1,
                cancel_boundary: None,
                cancelled_cell: Mutex::new(None),
                direct_probe: false,
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
                    "Submit a fixture proposal".into(),
                    TurnSubmissionOptions {
                        submit_change_batch: true,
                        ..TurnSubmissionOptions::default()
                    },
                )
                .await
                .unwrap();
            let completed = wait_for_kind(&kernel, &session.session_id, "turn_complete").await;
            assert_eq!(
                serde_json::from_str::<Value>(
                    completed["msg"]["last_agent_message"]
                        .as_str()
                        .expect("host handoff result")
                )
                .unwrap()["patch"],
                "fixture-patch"
            );
            {
                let requests = gate.0.lock().unwrap();
                assert!(
                    requests
                        .iter()
                        .any(|request| request.tool_name == "submit_change_batch")
                );
                assert!(
                    !requests
                        .iter()
                        .any(|request| request.tool_name == "exec_command")
                );
            }
            assert_eq!(
                model.requests.lock().unwrap().len(),
                1,
                "host handoff ends model sampling"
            );
            kernel.shutdown().await.unwrap();
        });
    });
}

#[test]
fn user_interrupt_closes_the_native_cell_before_the_next_turn() {
    assert_interrupt_closes_native_cell(CANCEL_SOURCE, false);
}

#[test]
fn user_interrupt_closes_a_native_cell_blocked_in_a_real_mcp_call() {
    assert_interrupt_closes_native_cell(
        r#"// @exec: {"yield_time_ms":1000}
const tool = ALL_TOOLS.find(t => t.name.endsWith('fixture__public_smoke'));
text(await tools[tool.name]({tag:'A', phase:'hold'}));
text(await tools[tool.name]({tag:'A', phase:'after'}));
"#,
        true,
    );
}

async fn assert_blocked_mcp_wait(fixture: &Fixture, kernel: &Kernel, session_id: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let started = std::fs::read_to_string(fixture.0.join("calls.jsonl"))
                .ok()
                .is_some_and(|oracle| {
                    oracle.lines().any(|line| {
                        serde_json::from_str::<Value>(line)
                            .ok()
                            .is_some_and(|observation| {
                                observation["phase"] == "hold" && observation["state"] == "started"
                            })
                    })
                });
            let facts = kernel
                .tool_runtime_events(session_id, 0, 200)
                .await
                .unwrap();
            if started
                && facts.iter().any(|event| {
                    let fact: Value = serde_json::from_str(&event.fact_json).unwrap();
                    fact["kind"] == "wait" && fact["fact"]["state"] == "waiting"
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("interrupt reaches a real blocked MCP call while functions.wait is active");
}

fn assert_interrupt_closes_native_cell(source: &'static str, blocked_mcp: bool) {
    run_native_test(move |runtime| {
        runtime.block_on(async {
            let fixture = Fixture::new();
            if blocked_mcp {
                fixture.configure_cancellation_mcp();
            }
            let boundary = Arc::new(tokio::sync::Notify::new());
            let model = Arc::new(ScriptedModel {
                requests: Mutex::new(Vec::new()),
                source,
                exec_count: 1,
                cancel_boundary: Some((
                    boundary.clone(),
                    if blocked_mcp {
                        CancelBoundary::ActiveWait
                    } else {
                        CancelBoundary::PendingModel
                    },
                )),
                cancelled_cell: Mutex::new(None),
                direct_probe: false,
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
            if blocked_mcp {
                assert_blocked_mcp_wait(&fixture, &kernel, &session.session_id).await;
            }
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
            kernel.close_session(&session.session_id).await.unwrap();
            let final_facts = kernel
                .tool_runtime_events(&session.session_id, 0, 200)
                .await
                .unwrap();
            let cell = model.cancelled_cell.lock().unwrap().clone().unwrap();
            let final_cell = final_facts
                .iter()
                .filter_map(|event| serde_json::from_str::<Value>(&event.fact_json).ok())
                .rfind(|fact| fact["kind"] == "cell" && fact["fact"]["cell_id"] == cell)
                .unwrap();
            assert_eq!(final_cell["fact"]["lifecycle"], "closed");
            kernel.shutdown().await.unwrap();
            if blocked_mcp {
                fixture.assert_no_mcp_phase("after");
            }
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
            assert!(
                last_input.contains("unavailable_wait"),
                "missing wait diagnosis: {last_input}"
            );
            assert!(last_input.contains("model_behavior_diagnosis"));
        });
    });
}

#[test]
fn native_cell_waits_for_user_input_and_rejects_direct_tool_calls() {
    run_native_test(|runtime| {
        runtime.block_on(async {
            let fixture = Fixture::new();
            let model = Arc::new(ScriptedModel {
                requests: Mutex::new(Vec::new()),
                source: QUESTION_SOURCE,
                exec_count: 1,
                cancel_boundary: None,
                cancelled_cell: Mutex::new(None),
                direct_probe: true,
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
                    "Ask through Code Mode".into(),
                    TurnSubmissionOptions::default(),
                )
                .await
                .unwrap();
            let question = wait_for_kind(&kernel, &session.session_id, "request_user_input").await;
            assert_eq!(question["msg"]["questions"][0]["id"], "decision");
            kernel
                .resolve_user_input(
                    &session.session_id,
                    question["msg"]["turn_id"].as_str().unwrap().to_owned(),
                    serde_json::from_value(json!({"answers":{"decision":{"answers":["Proceed"]}}}))
                        .unwrap(),
                )
                .await
                .unwrap();
            wait_for_kind(&kernel, &session.session_id, "turn_complete").await;
            kernel.shutdown().await.unwrap();
            let history = model_outputs(&model);
            for expected in [
                "must be invoked through Code Mode exec",
                "question-answer:",
                "Proceed",
                "question-after-yield",
            ] {
                assert!(history.contains(expected), "missing {expected}: {history}");
            }
            assert!(
                gate.0
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| request.operation_id != "direct-probe")
            );
            assert!(
                gate.0
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|request| request.tool_name == "request_user_input")
            );
        });
    });
}

async fn wait_for_kind(kernel: &Kernel, session_id: &str, kind: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match kernel.next_event(session_id, None).await.unwrap() {
                EventPoll::Event(event) if event.kind == kind => {
                    break serde_json::from_str(&event.payload_json).unwrap();
                }
                EventPoll::Event(event) => {
                    assert_ne!(event.kind, "error", "{}", event.payload_json);
                }
                EventPoll::Closed => panic!("session closed before {kind}"),
                EventPoll::Timeout => {}
            }
        }
    })
    .await
    .expect("expected Core event")
}

const DIAGNOSTIC_SOURCE: &str = r"
const echo = ALL_TOOLS.find(tool => tool.name.endsWith('fixture__echo'));
for (let i = 0; i < 6; i++) {
  try { await tools[echo.name]({value:'same-check'}); throw new Error('caught inside cell'); }
  catch {}
}
text('diagnosis-before-yield');
await yield_control();
text('diagnosis-cell-resumed');
";

#[test]
fn repeated_internal_calls_reach_current_model_at_yield_and_keep_cell_alive() {
    run_native_test(|runtime| {
        runtime.block_on(async {
            let fixture = Fixture::new();
            let model = Arc::new(ScriptedModel {
                requests: Mutex::new(Vec::new()),
                source: DIAGNOSTIC_SOURCE,
                exec_count: 1,
                cancel_boundary: None,
                cancelled_cell: Mutex::new(None),
                direct_probe: false,
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
                    "Assess repeated tool work".into(),
                    TurnSubmissionOptions::default(),
                )
                .await
                .unwrap();
            wait_for_kind(&kernel, &session.session_id, "turn_complete").await;
            let history = serde_json::to_string(&latest_exec_outputs(&model)).unwrap();
            assert!(history.contains("model_behavior_diagnosis"));
            assert!(history.contains("repeated_operation"));
            assert!(history.contains("diagnostic_id"));
            assert!(history.contains("diagnosis-cell-resumed"));
            assert_eq!(
                gate.0
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|request| request.tool_name.ends_with("echo"))
                    .count(),
                6
            );
            let facts = kernel
                .tool_runtime_events(&session.session_id, 0, 200)
                .await
                .unwrap();
            let facts: Vec<Value> = facts
                .iter()
                .map(|event| serde_json::from_str(&event.fact_json).unwrap())
                .collect();
            assert_model_diagnostic_sources(&model, &facts);
            assert!(
                facts
                    .iter()
                    .any(|fact| fact["kind"] == "diagnostic_response")
            );
            let diagnostics: Vec<_> = facts
                .iter()
                .filter(|fact| fact["kind"] == "diagnostic")
                .collect();
            assert_eq!(diagnostics.len(), 2);
            assert_eq!(diagnostics[0]["fact"]["delivery"], "queued");
            assert_eq!(diagnostics[1]["fact"]["delivery"], "offered");
            assert_eq!(
                diagnostics[0]["fact"]["diagnostic"]["diagnostic_id"],
                diagnostics[1]["fact"]["diagnostic"]["diagnostic_id"]
            );
            kernel.shutdown().await.unwrap();
        });
    });
}

const ALTERNATING_SOURCE: &str = r"
const echo = ALL_TOOLS.find(tool => tool.name.endsWith('fixture__echo'));
for (let i = 0; i < 6; i++) await tools[echo.name]({value: i % 2 === 0 ? 'state-a' : 'state-b'});
await yield_control();
text('alternating-cell-resumed');
";
const BRANCH_SOURCE: &str = r"
const echo = ALL_TOOLS.find(tool => tool.name.endsWith('fixture__echo'));
for (let i = 0; i < 4; i++) await tools[echo.name]({value:'branch-check'});
await yield_control();
text('branch-cell-resumed');
";

#[test]
fn alternating_operations_and_repeated_branches_reach_model_as_questions() {
    run_native_test(|runtime| {
        runtime.block_on(async {
            for (source, exec_count, expected, calls) in [
                (ALTERNATING_SOURCE, 1, "alternating_cycle", 6),
                (BRANCH_SOURCE, 3, "branch_expansion", 12),
            ] {
                let fixture = Fixture::new();
                let model = Arc::new(ScriptedModel {
                    requests: Mutex::new(Vec::new()),
                    source,
                    exec_count,
                    cancel_boundary: None,
                    cancelled_cell: Mutex::new(None),
                    direct_probe: false,
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
                        "Assess repeated work".into(),
                        TurnSubmissionOptions::default(),
                    )
                    .await
                    .unwrap();
                wait_for_kind(&kernel, &session.session_id, "turn_complete").await;
                let history = serde_json::to_string(&latest_exec_outputs(&model)).unwrap();
                assert!(history.contains(expected), "missing {expected}: {history}");
                assert!(history.contains("cell-resumed"));
                assert_eq!(
                    gate.0
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|request| request.tool_name.ends_with("echo"))
                        .count(),
                    calls
                );
                let facts = all_facts(&kernel, &session.session_id).await;
                assert_model_diagnostic_sources(&model, &facts);
                kernel.shutdown().await.unwrap();
            }
        });
    });
}

fn model_outputs(model: &ScriptedModel) -> String {
    model
        .requests
        .lock()
        .unwrap()
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
        .join("\n")
}

/// Read one current exec and its following waits from the final model request.
/// Earlier tool outputs remain in Core history across turns and cannot prove this delivery.
fn latest_exec_outputs(model: &ScriptedModel) -> Vec<Value> {
    let requests = model.requests.lock().unwrap();
    let input = requests.last().unwrap()["request"]["input"]
        .as_array()
        .unwrap();
    let latest = input
        .iter()
        .rposition(|item| item["type"] == "custom_tool_call" && item["name"] == "exec")
        .expect("latest native exec call");
    input[latest + 1..]
        .iter()
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("custom_tool_call_output" | "function_call_output")
            )
        })
        .map(|item| item["output"].clone())
        .collect()
}

/// Resolve nested requests through Core's exact parent request and cell scope.
fn latest_exec_requests(facts: &[Value]) -> std::collections::BTreeMap<i64, &Value> {
    let boundary = facts
        .iter()
        .filter(|fact| {
            fact["kind"] == "request"
                && matches!(
                    fact["fact"]["request"]["tool_name"].as_str(),
                    Some("exec" | "functions.exec")
                )
        })
        .map(|fact| fact["fact"]["request_sequence"].as_i64().unwrap())
        .max()
        .expect("current Core exec request");
    let cells = facts
        .iter()
        .filter(|fact| {
            fact["kind"] == "cell" && fact["fact"]["parent_request_sequence"] == boundary
        })
        .map(|fact| {
            (
                fact["fact"]["cell_id"].as_str().unwrap(),
                fact["fact"]["scope_id"].as_str().unwrap(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert!(!cells.is_empty(), "current exec must own a Core cell");
    let requests = facts
        .iter()
        .filter(|fact| {
            fact["kind"] == "request"
                && fact["fact"]["request"]["source"] == "code_mode"
                && cells.contains(&(
                    fact["fact"]["request"]["cell_id"].as_str().unwrap(),
                    fact["fact"]["request"]["scope_id"].as_str().unwrap(),
                ))
        })
        .map(|fact| {
            (
                fact["fact"]["request_sequence"].as_i64().unwrap(),
                &fact["fact"],
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert!(
        !requests.is_empty(),
        "current exec must own nested Core requests"
    );
    requests
}

async fn all_facts(kernel: &Kernel, session: &str) -> Vec<Value> {
    let mut cursor = 0;
    let mut facts = Vec::new();
    loop {
        let page = kernel
            .tool_runtime_events(session, cursor, 200)
            .await
            .unwrap();
        if page.is_empty() {
            return facts;
        }
        cursor = page.last().unwrap().source_sequence;
        facts.extend(
            page.into_iter()
                .map(|event| serde_json::from_str(&event.fact_json).unwrap()),
        );
    }
}

fn assert_model_diagnostic_sources(model: &ScriptedModel, facts: &[Value]) {
    fn collect(value: &Value, diagnostics: &mut Vec<Value>) {
        match value {
            Value::String(text) => {
                if let Some((_, tail)) = text.split_once("<model_behavior_diagnosis>") {
                    let body = tail.split_once("</model_behavior_diagnosis>").unwrap().0;
                    let fragment: Value = serde_json::from_str(body).unwrap();
                    assert_eq!(fragment["type"], "model_behavior_diagnosis");
                    diagnostics.extend(fragment["diagnostics"].as_array().unwrap().iter().cloned());
                }
            }
            Value::Array(values) => values.iter().for_each(|value| collect(value, diagnostics)),
            Value::Object(values) => values
                .values()
                .for_each(|value| collect(value, diagnostics)),
            _ => {}
        }
    }
    let mut diagnostics = Vec::new();
    for output in latest_exec_outputs(model) {
        collect(&output, &mut diagnostics);
    }
    assert!(
        !diagnostics.is_empty(),
        "model received no Core diagnosis in latest exec"
    );
    let current_requests = latest_exec_requests(facts);
    for diagnostic in diagnostics {
        let durable = facts
            .iter()
            .find(|fact| {
                fact["kind"] == "diagnostic"
                    && fact["fact"]["delivery"] == "offered"
                    && fact["fact"]["diagnostic"]["diagnostic_id"] == diagnostic["diagnostic_id"]
                    && fact["fact"]["diagnostic"]["evidence_version"]
                        == diagnostic["evidence_version"]
            })
            .expect("model diagnosis must identify an offered Core fact");
        let calls = diagnostic["calls"].as_array().unwrap();
        // Branch evidence may refer to other branches, but this question must
        // include at least one original request from the current exec scope.
        assert!(
            calls
                .iter()
                .any(|call| current_requests
                    .contains_key(&call["request_sequence"].as_i64().unwrap())),
            "latest exec diagnosis must include current scope evidence"
        );
        assert_eq!(
            calls.len(),
            durable["fact"]["diagnostic"]["evidence"]
                .as_array()
                .unwrap()
                .len()
                .min(2)
        );
        if diagnostic["kind"] == "branch_expansion" {
            for field in ["parent_call_id", "cell_id"] {
                let branches = calls
                    .iter()
                    .map(|call| call[field].as_str().expect("original branch identity"))
                    .collect::<std::collections::BTreeSet<_>>();
                assert!(branches.len() >= 2, "branch diagnosis collapsed {field}");
            }
        }
        for call in calls {
            let sequence = &call["request_sequence"];
            let request = facts
                .iter()
                .rfind(|fact| {
                    fact["kind"] == "request" && fact["fact"]["request_sequence"] == *sequence
                })
                .expect("diagnosis must identify an original Core request");
            let operation = facts
                .iter()
                .rfind(|fact| {
                    fact["kind"] == "sharing" && fact["fact"]["request_sequence"] == *sequence
                })
                .map_or(&request["fact"]["attempt"]["operation_digest"], |sharing| {
                    &sharing["fact"]["operation_digest"]
                });
            assert!(operation.as_str().is_some_and(|digest| digest.len() == 64));
            let evidence = durable["fact"]["diagnostic"]["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .find(|evidence| evidence["request_sequence"] == *sequence)
                .expect("model diagnosis must preserve its Core evidence");
            assert_eq!(call["tool"], request["fact"]["request"]["tool_name"]);
            assert_eq!(
                call["parent_call_id"],
                request["fact"]["request"]["parent_call_id"]
            );
            assert_eq!(call["cell_id"], request["fact"]["request"]["cell_id"]);
            assert_eq!(call["operation_digest"], *operation);
            assert_eq!(evidence["operation_digest"], *operation);
        }
    }
}

#[test]
fn model_outputs_ignore_script_markers_until_they_are_returned() {
    for (source, marker) in [
        (DIAGNOSTIC_SOURCE, "diagnosis-cell-resumed"),
        (ALTERNATING_SOURCE, "alternating-cell-resumed"),
        (BRANCH_SOURCE, "branch-cell-resumed"),
        (QUESTION_SOURCE, "question-after-yield"),
    ] {
        let model = ScriptedModel {
            requests: Mutex::new(vec![json!({"request":{"input":[
                {"type":"custom_tool_call","name":"exec","input":source},
                {"type":"custom_tool_call_output","output":"Script terminated before continuation"},
                {"type":"function_call_output","output":"returned function output"},
            ]}})]),
            source,
            exec_count: 1,
            cancel_boundary: None,
            cancelled_cell: Mutex::new(None),
            direct_probe: false,
        };
        assert!(
            model.requests.lock().unwrap()[0]
                .to_string()
                .contains(marker)
        );
        assert_eq!(
            model_outputs(&model),
            "\"Script terminated before continuation\"\n\"returned function output\""
        );
        assert!(!model_outputs(&model).contains(marker));
        assert!(
            !serde_json::to_string(&latest_exec_outputs(&model))
                .unwrap()
                .contains(marker)
        );
    }
}

/// A valid old diagnosis and receipt remain in history after a new exec starts.
fn historical_feedback_fixture() -> (ScriptedModel, Vec<Value>) {
    let operation = "a".repeat(64);
    let call = json!({"request_sequence":2,"tool":"mcp.fixture.public_smoke",
        "parent_call_id":"old-exec","cell_id":"old-cell","operation_digest":operation});
    let diagnosis = json!({"diagnostic_id":"d".repeat(64),"evidence_version":2,
        "kind":"repeated_operation","calls":[call]});
    let diagnosis_output = format!(
        "<model_behavior_diagnosis>{}</model_behavior_diagnosis>",
        json!({"type":"model_behavior_diagnosis","diagnostics":[diagnosis]})
    );
    let receipt_output = format!(
        "<core_tool_receipts>{}</core_tool_receipts>",
        json!({"receipts":[{"request_sequence":2,"source_id":"old-request",
            "tool":"mcp.fixture.public_smoke","execution":"completed",
            "disposition":"accepted","delivery":"offered","source_request_sequence":null}]})
    );
    let old_request = json!({"kind":"request","fact":{"request_sequence":2,
        "request":{"logical_id":"old-request","tool_name":"mcp.fixture.public_smoke",
            "source":"code_mode","parent_call_id":"old-exec","cell_id":"old-cell","scope_id":"old-scope"},
        "attempt":{"operation_digest":operation,"execution":"completed","disposition":"accepted","delivery":"offered"}}});
    let mut current_request = old_request.clone();
    current_request["fact"]["request_sequence"] = json!(4);
    for (field, value) in [
        ("logical_id", "current-request"),
        ("parent_call_id", "current-exec"),
        ("cell_id", "current-cell"),
        ("scope_id", "current-scope"),
    ] {
        current_request["fact"]["request"][field] = json!(value);
    }
    let mut durable = diagnosis;
    durable["evidence"] = json!([call]);
    let facts = vec![
        old_request,
        json!({"kind":"diagnostic","fact":{"delivery":"offered","diagnostic":durable}}),
        json!({"kind":"request","fact":{"request_sequence":3,"request":{"tool_name":"exec"}}}),
        json!({"kind":"cell","fact":{"parent_request_sequence":3,"cell_id":"current-cell","scope_id":"current-scope"}}),
        current_request,
    ];
    let model = ScriptedModel {
        requests: Mutex::new(vec![json!({"request":{"input":[
            {"type":"custom_tool_call","name":"exec","call_id":"old-exec","input":"old source"},
            {"type":"custom_tool_call_output","call_id":"old-exec","output":[diagnosis_output,receipt_output]},
            {"type":"custom_tool_call","name":"exec","call_id":"current-exec","input":"current source"},
            {"type":"custom_tool_call_output","call_id":"current-exec","output":"current execution output"}
        ]}})]),
        source: "",
        exec_count: 1,
        cancel_boundary: None,
        cancelled_cell: Mutex::new(None),
        direct_probe: false,
    };
    (model, facts)
}

#[test]
#[should_panic(expected = "model received no Core diagnosis in latest exec")]
fn historical_diagnosis_cannot_substitute_for_latest_exec_feedback() {
    let (model, facts) = historical_feedback_fixture();
    assert!(model_outputs(&model).contains("model_behavior_diagnosis"));
    assert_model_diagnostic_sources(&model, &facts);
}

#[test]
#[should_panic(expected = "latest exec diagnosis must include current scope evidence")]
fn republished_historical_diagnosis_cannot_substitute_for_current_scope_evidence() {
    let (model, facts) = historical_feedback_fixture();
    {
        let mut requests = model.requests.lock().unwrap();
        let input = requests[0]["request"]["input"].as_array_mut().unwrap();
        input[3]["output"] = input[1]["output"].clone();
    }
    assert_model_diagnostic_sources(&model, &facts);
}

#[path = "code_mode_sharing.rs"]
mod sharing;

#[path = "code_mode_recovery.rs"]
mod recovery;

#[path = "code_mode_agent_wait.rs"]
mod agent_wait;
