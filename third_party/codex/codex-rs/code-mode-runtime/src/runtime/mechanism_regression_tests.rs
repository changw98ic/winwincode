use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::alloc::System;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode_protocol::*;
use codex_protocol::ToolName;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::PendingRuntimeMode;
use super::RuntimeCommand;
use super::RuntimeEvent;
use super::spawn_runtime;
use crate::InProcessCodeModeSession;

static PENDING_CHECKS: AtomicUsize = AtomicUsize::new(0);
static PENDING_ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COMPLETION_ALLOCATION_BYTES: Cell<Option<usize>> = const { Cell::new(None) };
}

struct ObservedAllocator;

#[global_allocator]
static ALLOCATOR: ObservedAllocator = ObservedAllocator;

unsafe impl GlobalAlloc for ObservedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COMPLETION_ALLOCATION_BYTES.with(|bytes| {
            if let Some(count) = bytes.get() {
                bytes.set(Some(count.saturating_add(layout.size())));
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        COMPLETION_ALLOCATION_BYTES.with(|bytes| {
            if let Some(count) = bytes.get() {
                bytes.set(Some(count.saturating_add(size)));
            }
        });
        unsafe { System.realloc(pointer, layout, size) }
    }
}

// Measure the real Rust allocator only while completion_state executes on its V8 thread.
pub(super) struct CompletionAllocationProbe;

impl CompletionAllocationProbe {
    pub(super) fn new() -> Self {
        COMPLETION_ALLOCATION_BYTES.with(|bytes| bytes.set(Some(0)));
        Self
    }

    pub(super) fn record_pending(&self) {
        PENDING_CHECKS.fetch_add(1, Ordering::SeqCst);
        COMPLETION_ALLOCATION_BYTES.with(|bytes| {
            PENDING_ALLOCATED_BYTES.fetch_add(bytes.get().unwrap_or_default(), Ordering::SeqCst);
        });
    }
}

impl Drop for CompletionAllocationProbe {
    fn drop(&mut self) {
        COMPLETION_ALLOCATION_BYTES.with(|bytes| bytes.set(None));
    }
}

fn request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "synthetic-mechanism-cell".to_string(),
        enabled_tools: Vec::new(),
        source: source.to_string(),
        yield_time_ms: Some(100),
        max_output_tokens: None,
    }
}

fn thread_count() -> usize {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc/self/task").unwrap().count()
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/bin/ps")
            .args(["-M", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .skip(1)
            .filter(|line| !line.trim().is_empty())
            .count()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        panic!("thread count fixture supports release-target macOS and Linux")
    }
}

#[derive(Default)]
struct CloseDelegate {
    closed: AtomicUsize,
}
impl CodeModeSessionDelegate for CloseDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async { Ok(Value::Null) })
    }
    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }
    fn cell_closed(&self, _cell_id: &CellId) {
        self.closed.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn mechanism_m16_cell_close_and_cancel_retire_timer_threads() {
    // Warm V8's process-wide thread pool before measuring timer resources.
    let warm = InProcessCodeModeSession::new();
    warm.execute(request("text('warm');"))
        .await
        .unwrap()
        .initial_response()
        .await
        .unwrap();
    warm.shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut excess = Vec::new();
    for mode in ["clear_then_close", "close_without_clear", "terminate"] {
        let baseline = thread_count();
        let delegate = Arc::new(CloseDelegate::default());
        let session = InProcessCodeModeSession::with_delegate(delegate.clone());
        let suffix = match mode {
            "clear_then_close" => "for (const id of ids) clearTimeout(id);",
            "close_without_clear" => "text('closed');",
            "terminate" => "await new Promise(() => {});",
            _ => unreachable!(),
        };
        let source = format!(
            "const ids = []; for (let i=0; i<32; i++) ids.push(setTimeout(() => {{}}, 5000)); {suffix}"
        );
        let started = session.execute(request(&source)).await.unwrap();
        let cell_id = started.cell_id.clone();
        let response = tokio::time::timeout(Duration::from_secs(2), started.initial_response())
            .await
            .unwrap()
            .unwrap();
        if mode == "terminate" {
            assert!(matches!(response, RuntimeResponse::Yielded { .. }));
            tokio::time::timeout(Duration::from_secs(2), session.terminate(cell_id))
                .await
                .unwrap()
                .unwrap();
        } else {
            assert!(matches!(
                response,
                RuntimeResponse::Result {
                    error_text: None,
                    ..
                }
            ));
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while delegate.closed.load(Ordering::SeqCst) != 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        session.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let after_closed = thread_count();
        // Always allow owned timers to expire before any red assertion.
        tokio::time::sleep(Duration::from_millis(5400)).await;
        let after_expiry = thread_count();
        let row = serde_json::json!({"id":"M16","mode":mode,"timers":32,"delay_ms":5000,"baseline_os_threads":baseline,"after_cell_closed_os_threads":after_closed,"after_original_expiry_os_threads":after_expiry,"cell_closed_callbacks":delegate.closed.load(Ordering::SeqCst),"source":"actual InProcessCodeModeSession + V8 timers"});
        eprintln!("MECHANISM_RECEIPT {row}");
        assert!(
            after_expiry <= baseline + 8,
            "owned timer threads did not expire within cleanup bound"
        );
        excess.push(after_closed.saturating_sub(baseline));
    }
    assert!(
        excess.iter().all(|threads| *threads <= 8),
        "cell close/cancel must promptly retire timers; extra OS threads {excess:?}"
    );
}

async fn pending_store(bytes: usize) -> (usize, usize, usize) {
    PENDING_CHECKS.store(0, Ordering::SeqCst);
    PENDING_ALLOCATED_BYTES.store(0, Ordering::SeqCst);
    let source = format!(
        "{} for (let i=0; i<32; i++) await tools.echo({{}});",
        if bytes == 0 {
            String::new()
        } else {
            format!("store('synthetic', 'x'.repeat({bytes}));")
        }
    );
    let mut execute = request(&source);
    execute.enabled_tools = vec![ToolDefinition {
        name: "echo".to_string(),
        tool_name: ToolName::plain("echo"),
        description: String::new(),
        kind: CodeModeToolKind::Function,
        input_schema: None,
        output_schema: None,
    }];
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (command_tx, _control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        execute,
        event_tx,
        PendingRuntimeMode::Continue,
        None,
    )
    .unwrap();
    let mut replies = 0;
    let mut pending_call = None;
    let mut final_bytes = 0;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .unwrap()
    {
        match event {
            RuntimeEvent::ToolCall { id, .. } => {
                pending_call = Some(id);
            }
            RuntimeEvent::Pending => {
                if let Some(id) = pending_call.take() {
                    command_tx
                        .send(RuntimeCommand::ToolResponse {
                            id,
                            result: Value::Null,
                        })
                        .unwrap();
                    replies += 1;
                }
            }
            RuntimeEvent::Result {
                stored_value_writes,
                error_text,
            } => {
                assert_eq!(error_text, None);
                final_bytes = stored_value_writes
                    .get("synthetic")
                    .and_then(Value::as_str)
                    .map_or(0, str::len);
            }
            RuntimeEvent::ThreadPanicked => panic!("real V8 runtime panicked"),
            RuntimeEvent::Started
            | RuntimeEvent::ContentItem(_)
            | RuntimeEvent::YieldRequested
            | RuntimeEvent::Notify { .. } => {}
        }
    }
    assert_eq!(replies, 32);
    assert_eq!(final_bytes, bytes);
    (
        PENDING_CHECKS.load(Ordering::SeqCst),
        PENDING_ALLOCATED_BYTES.load(Ordering::SeqCst),
        replies,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn mechanism_m17_pending_does_not_deep_clone_store() {
    let baseline = pending_store(0).await;
    let with_store = pending_store(1024 * 1024).await;
    for (label, sample) in [("baseline", baseline), ("store_1mib", with_store)] {
        let row = serde_json::json!({"id":"M17","case":label,"pending_checks":sample.0,"pending_allocated_bytes":sample.1,"actual_tool_replies":sample.2,"source":"actual spawn_runtime + completion_state Rust allocator observation"});
        eprintln!("MECHANISM_RECEIPT {row}");
    }
    assert_eq!(
        with_store.1, 0,
        "Pending completion must avoid deep cloning unmodified stored values"
    );
    assert!(
        with_store.0 >= 32,
        "the allocator probe must observe the actual Pending checks"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn timer_clear_does_not_invoke_callback_and_new_timer_fires() {
    let session = InProcessCodeModeSession::new();
    let response = session.execute(ExecuteRequest {
        yield_time_ms: Some(60_000),
        ..request(
            "const id = setTimeout(() => text('cancelled callback'), 20); clearTimeout(id); await new Promise(resolve => setTimeout(resolve, 40)); text('completed');"
        )
    }).await.unwrap().initial_response().await.unwrap();
    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: CellId::new("1".to_string()),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "completed".to_string()
            }],
            error_text: None,
        }
    );
    session.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn final_store_writes_commit_on_error_and_are_discarded_on_cancel() {
    let session = InProcessCodeModeSession::new();
    let response = session
        .execute(request(
            "store('value', 'committed'); throw new Error('expected error');",
        ))
        .await
        .unwrap()
        .initial_response()
        .await
        .unwrap();
    assert!(matches!(
        response,
        RuntimeResponse::Result {
            error_text: Some(_),
            ..
        }
    ));
    let started = session
        .execute(request(
            "store('value', 'cancelled'); await new Promise(() => {});",
        ))
        .await
        .unwrap();
    let cell_id = started.cell_id.clone();
    assert!(matches!(
        started.initial_response().await.unwrap(),
        RuntimeResponse::Yielded { .. }
    ));
    session.terminate(cell_id).await.unwrap();
    let response = session
        .execute(request("text(load('value')); "))
        .await
        .unwrap()
        .initial_response()
        .await
        .unwrap();
    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: CellId::new("3".to_string()),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "committed".to_string()
            }],
            error_text: None,
        }
    );
    session.shutdown().await.unwrap();
}
