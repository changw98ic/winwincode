// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};
use winwincode_execution_port::agent_config::{AgentProfileSettings, resolve_agent_session_config};
use winwincode_execution_port::task_handoff::TASK_HANDOFF_COMPACT_PROMPT;

use crate::{
    EventPoll, Kernel, KernelOptions, ModelPort, ModelPortFailure, ModelPortRequest,
    ModelPortStream, Op, RejectingKernelActionGate, SessionOptions, TurnSubmissionOptions,
};

const HANDOFF: &str = "Task: fixture\nStatus: in_progress\nWorkspace: fixture workspace\nValidation: unverified\nChanges: {}\nDependencies: []\nUnverified: [acceptance result missing]";
const LATEST_SCOPE: &str = "Current user scope: only update the assigned input path.";
const BOUNDARY: &str = "task-boundary-marker";

fn contains_compact_prompt(payload: &Value) -> bool {
    payload["request"]["input"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["content"].as_array().is_some_and(|content| {
                content
                    .iter()
                    .any(|part| part["text"] == TASK_HANDOFF_COMPACT_PROMPT)
            })
        })
    })
}

#[derive(Debug)]
struct HandoffModel {
    requests: Mutex<Vec<Value>>,
    handoff: String,
    host_context: String,
}

impl Default for HandoffModel {
    fn default() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            handoff: HANDOFF.replace(
                "Changes: {}",
                &format!(
                    "Changes: {}",
                    json!({
                        "src/fixture.rs": "observed file state ".repeat(800),
                    })
                ),
            ),
            host_context: "host execution binding ".repeat(800),
        }
    }
}

impl ModelPort for HandoffModel {
    fn compaction_context_overhead_bytes(
        &self,
        _thread_id: &str,
    ) -> Result<usize, ModelPortFailure> {
        Ok(self.host_context.len())
    }

    fn stream(
        &self,
        request: ModelPortRequest,
    ) -> BoxFuture<'static, Result<ModelPortStream, ModelPortFailure>> {
        let mut payload: Value = serde_json::from_str(&request.payload_json).unwrap();
        let instructions = payload["request"]["instructions"]
            .as_str()
            .unwrap_or_default();
        payload["request"]["instructions"] = json!(format!("{instructions}{}", self.host_context));
        let mut requests = self.requests.lock().unwrap();
        let index = requests.len();
        let text = if contains_compact_prompt(&payload) {
            self.handoff.as_str()
        } else {
            "observed"
        };
        requests.push(payload);
        let frames = [
            json!({"type":"created"}).to_string(),
            json!({"type":"output_item_done", "item":{
                "type":"message", "role":"assistant", "phase":"final_answer",
                "content":[{"type":"output_text","text":text}]
            }})
            .to_string(),
            json!({"type":"completed","responseId":format!("fixture-{index}"),"endTurn":true})
                .to_string(),
        ];
        Box::pin(async move {
            Ok(Box::pin(futures::stream::iter(frames.into_iter().map(Ok))) as ModelPortStream)
        })
    }
}

fn session_options(workspace: std::path::PathBuf) -> SessionOptions {
    let worker = serde_json::from_value(json!("wrk_handoff_fixture")).unwrap();
    let capabilities = serde_json::from_value(json!({
        "capabilityDigest":format!("sha256:{}", "a".repeat(64)),
        "features":["sandbox"],"maxConcurrentJobs":1,"platform":"aarch64-apple-darwin"
    }))
    .unwrap();
    SessionOptions {
        cwd: workspace,
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

async fn wait_for_completion(kernel: &Kernel, session: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match kernel.next_event(session, None).await.unwrap() {
                EventPoll::Event(event) if event.kind == "turn_complete" => break,
                EventPoll::Event(event) => {
                    assert_ne!(event.kind, "error", "{}", event.payload_json);
                }
                EventPoll::Closed => panic!("closed before completion"),
                EventPoll::Timeout => {}
            }
        }
    })
    .await
    .expect("completed Core turn");
}

#[test]
fn compact_request_and_resumed_turn_use_task_state() {
    run_core_fixture(|| Box::pin(compact_and_resume_task_state()));
}

fn run_core_fixture(test: fn() -> BoxFuture<'static, ()>) {
    // Match the native Core fixture's caller and worker stack sizes.
    std::thread::Builder::new()
        .name("task-state-compaction-test".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(test());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn compact_and_resume_task_state() {
    let root =
        std::env::temp_dir().join(format!("winwincode-task-compaction-{}", std::process::id()));
    let workspace = root.join("workspace");
    let home = root.join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("AGENTS.md"), BOUNDARY).unwrap();
    // Device configuration must not select a Core compaction path that skips
    // the product task-state prompt.
    std::fs::write(
        home.join("config.toml"),
        "[features]\ntoken_budget = true\n",
    )
    .unwrap();
    let model = Arc::new(HandoffModel::default());
    let kernel = Kernel::new(
        KernelOptions::new(home, std::env::current_exe().unwrap()),
        model.clone(),
        Arc::new(RejectingKernelActionGate),
    )
    .unwrap();
    let options = session_options(workspace);
    let session = kernel.create_session(options.clone()).await.unwrap();
    let transcript = "older transcript material ".repeat(20_000);
    kernel
        .submit_turn(
            &session.session_id,
            transcript.clone(),
            TurnSubmissionOptions::default(),
        )
        .await
        .unwrap();
    wait_for_completion(&kernel, &session.session_id).await;
    kernel
        .submit_turn(
            &session.session_id,
            LATEST_SCOPE.into(),
            TurnSubmissionOptions::default(),
        )
        .await
        .unwrap();
    wait_for_completion(&kernel, &session.session_id).await;
    let runtime = kernel.runtime().await.unwrap();
    let live = kernel.session(&runtime, &session.session_id).await.unwrap();
    live.thread.submit(Op::Compact).await.unwrap();
    wait_for_completion(&kernel, &session.session_id).await;
    kernel
        .submit_turn(
            &session.session_id,
            "Continue this task.".into(),
            TurnSubmissionOptions::default(),
        )
        .await
        .unwrap();
    wait_for_completion(&kernel, &session.session_id).await;
    live.thread.ensure_rollout_materialized().await;
    live.thread.flush_rollout().await.unwrap();
    let rollout = live.thread.rollout_path().unwrap();
    drop(live);
    drop(runtime);
    kernel.close_session(&session.session_id).await.unwrap();
    let resumed = kernel.resume_session(rollout, options).await.unwrap();
    kernel
        .submit_turn(
            &resumed.session_id,
            "Continue after reopen.".into(),
            TurnSubmissionOptions::default(),
        )
        .await
        .unwrap();
    wait_for_completion(&kernel, &resumed.session_id).await;
    assert_restored_task_state(&model, &transcript);
    kernel.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

fn assert_restored_task_state(model: &HandoffModel, transcript: &str) {
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert!(model.handoff.len() > 900 * 4);
    assert!(
        !TASK_HANDOFF_COMPACT_PROMPT
            .chars()
            .any(|character| character.is_ascii_digit())
    );
    for request in &requests[..2] {
        assert!(request.to_string().contains(BOUNDARY));
    }
    assert!(contains_compact_prompt(&requests[2]));
    for request in &requests[3..] {
        let input = request["request"]["input"].to_string();
        assert!(input.contains("acceptance result missing"));
        assert!(input.contains(LATEST_SCOPE));
        assert!(!input.contains(transcript));
        assert!(input.len() < transcript.len() / 2);
        assert!(input.matches("older transcript material").count() > 500);
        assert!(
            request["request"]["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| {
                    item["content"].as_array().is_some_and(|content| {
                        content.iter().any(|part| {
                            part["text"]
                                .as_str()
                                .is_some_and(|text| text.contains(&model.handoff))
                        })
                    })
                })
        );
        // These requests add new user/assistant turns to the checkpoint.
        assert!(request_context_tokens(request) <= 30_000 + 512);
        let mut checkpoint = request.clone();
        checkpoint["request"]["input"]
            .as_array_mut()
            .unwrap()
            .retain(|item| {
                !item["content"].as_array().is_some_and(|content| {
                    content.iter().any(|part| {
                        matches!(
                            part["text"].as_str(),
                            Some("Continue this task." | "Continue after reopen." | "observed")
                        )
                    })
                })
            });
        // Independently count the restored checkpoint including host text,
        // tools and serialized wrappers, before the continuation adds turns.
        let checkpoint_tokens = request_context_tokens(&checkpoint);
        assert!(checkpoint_tokens <= 30_000, "{checkpoint_tokens}");
        assert!(checkpoint_tokens > 29_000, "{checkpoint_tokens}");
        assert!(request.to_string().contains(BOUNDARY));
    }
}

fn request_context_tokens(payload: &Value) -> usize {
    let request = &payload["request"];
    let instructions = request["instructions"]
        .as_str()
        .unwrap_or_default()
        .len()
        .div_ceil(4);
    let input = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.to_string().len().div_ceil(4))
        .sum::<usize>();
    instructions + input + request["tools"].to_string().len().div_ceil(4)
}

#[test]
fn oversized_task_state_preserves_original_history() {
    run_core_fixture(|| {
        Box::pin(assert_rejected_budget(
            HANDOFF.replace(
                "Task: fixture",
                &format!("Task: {}", "required task detail ".repeat(8_000)),
            ),
            String::new(),
            "oversized-state",
        ))
    });
}

#[test]
fn restored_host_context_is_included_in_total_budget() {
    run_core_fixture(|| {
        Box::pin(assert_rejected_budget(
            HANDOFF.into(),
            "host binding ".repeat(12_000),
            "oversized-host",
        ))
    });
}

async fn assert_rejected_budget(handoff: String, host_context: String, label: &str) {
    const ORIGINAL: &str = "Original conversation must survive a rejected compaction.";
    let root = std::env::temp_dir().join(format!("winwincode-{label}-{}", std::process::id()));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let model = Arc::new(HandoffModel {
        requests: Mutex::new(Vec::new()),
        handoff,
        host_context,
    });
    let kernel = Kernel::new(
        KernelOptions::new(root.join("home"), std::env::current_exe().unwrap()),
        model.clone(),
        Arc::new(RejectingKernelActionGate),
    )
    .unwrap();
    let session = kernel
        .create_session(session_options(workspace))
        .await
        .unwrap();
    kernel
        .submit_turn(
            &session.session_id,
            ORIGINAL.into(),
            TurnSubmissionOptions::default(),
        )
        .await
        .unwrap();
    wait_for_completion(&kernel, &session.session_id).await;
    let runtime = kernel.runtime().await.unwrap();
    let live = kernel.session(&runtime, &session.session_id).await.unwrap();
    live.thread.submit(Op::Compact).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match kernel.next_event(&session.session_id, None).await.unwrap() {
                EventPoll::Event(event) if event.kind == "error" => {
                    assert!(event.payload_json.contains("internal context budget"));
                    break;
                }
                EventPoll::Event(event) => {
                    assert_ne!(event.kind, "turn_complete", "budget violation was accepted");
                }
                EventPoll::Closed => panic!("session closed before budget validation"),
                EventPoll::Timeout => {}
            }
        }
    })
    .await
    .unwrap();
    wait_for_completion(&kernel, &session.session_id).await;
    live.thread.ensure_rollout_materialized().await;
    live.thread.flush_rollout().await.unwrap();
    let rollout = std::fs::read_to_string(live.thread.rollout_path().unwrap()).unwrap();
    assert!(
        !rollout
            .lines()
            .any(|line| serde_json::from_str::<Value>(line).unwrap()["type"] == "compacted")
    );
    kernel
        .submit_turn(
            &session.session_id,
            "Continue after the rejected checkpoint.".into(),
            TurnSubmissionOptions::default(),
        )
        .await
        .unwrap();
    wait_for_completion(&kernel, &session.session_id).await;
    assert!(
        model.requests.lock().unwrap().last().unwrap()["request"]["input"]
            .to_string()
            .contains(ORIGINAL)
    );
    kernel.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
