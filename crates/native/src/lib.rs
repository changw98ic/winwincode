//! Node-API boundary for the embedded Codex kernel.

#![recursion_limit = "256"]

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use futures::Stream;
use napi::Error;
use napi::Result;
use napi::Status;
use napi::bindgen_prelude::FnArgs;
use napi::bindgen_prelude::Function;
use napi::bindgen_prelude::ReadableStream;
use napi::bindgen_prelude::Reader;
use napi::bindgen_prelude::spawn as napi_spawn;
use napi::threadsafe_function::ThreadsafeFunction;
use napi::threadsafe_function::ThreadsafeFunctionCallMode;
use napi_derive::module_init;
use napi_derive::napi;
use tokio::runtime::Builder;
use winwincode_kernel::ApprovalDecision as KernelApprovalDecision;
use winwincode_kernel::ApprovalKind as KernelApprovalKind;
use winwincode_kernel::ApprovalResponse as KernelApprovalResponse;
use winwincode_kernel::DynamicToolCallResponse as KernelDynamicToolCallResponse;
use winwincode_kernel::EventPoll;
use winwincode_kernel::ForkOptions as KernelForkOptions;
use winwincode_kernel::Kernel;
use winwincode_kernel::KernelBuildInfo;
use winwincode_kernel::KernelEvent;
use winwincode_kernel::KernelFailure;
use winwincode_kernel::KernelOptions;
use winwincode_kernel::ModelPort;
use winwincode_kernel::ModelPortFailure;
use winwincode_kernel::ModelPortRequest;
use winwincode_kernel::ModelPortStream;
use winwincode_kernel::SessionInfo;
use winwincode_kernel::SessionOptions;
use winwincode_kernel::ShutdownInfo;
use winwincode_kernel::SubmissionInfo;

const ERROR_PREFIX: &str = "WINWINCODE_KERNEL_ERROR";
const CODEX_TOKIO_WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

type ModelStreamCallback = ThreadsafeFunction<
    String,
    ReadableStream<'static, String>,
    FnArgs<(String,)>,
    Status,
    false,
    true,
>;
type ModelCancelCallback = ThreadsafeFunction<String, (), FnArgs<(String,)>, Status, false, true>;

struct NativeModelPort {
    stream_callback: Arc<ModelStreamCallback>,
    cancel_callback: Arc<ModelCancelCallback>,
}

impl std::fmt::Debug for NativeModelPort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeModelPort")
            .finish_non_exhaustive()
    }
}

struct NativeModelStream {
    request_id: String,
    reader: Reader<String>,
    cancel_callback: Arc<ModelCancelCallback>,
    finished: bool,
}

impl Stream for NativeModelStream {
    type Item = std::result::Result<String, ModelPortFailure>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.reader).poll_next(context) {
            Poll::Ready(Some(Ok(message))) => Poll::Ready(Some(Ok(message))),
            Poll::Ready(Some(Err(error))) => {
                self.finished = true;
                Poll::Ready(Some(Err(ModelPortFailure::new(
                    "MODEL_PORT_STREAM_FAILED",
                    error.to_string(),
                ))))
            }
            Poll::Ready(None) => {
                self.finished = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for NativeModelStream {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let _ = self.cancel_callback.call(
            self.request_id.clone(),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
    }
}

impl ModelPort for NativeModelPort {
    fn stream(
        &self,
        request: ModelPortRequest,
    ) -> futures::future::BoxFuture<'static, std::result::Result<ModelPortStream, ModelPortFailure>>
    {
        let stream_callback = self.stream_callback.clone();
        let cancel_callback = self.cancel_callback.clone();
        Box::pin(async move {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let status = stream_callback.call_with_return_value(
                request.payload_json,
                ThreadsafeFunctionCallMode::NonBlocking,
                move |result, _env| {
                    let result = result
                        .and_then(|stream| stream.read())
                        .map_err(|error| error.to_string());
                    let _ = sender.send(result);
                    Ok(())
                },
            );
            if status != Status::Ok {
                return Err(ModelPortFailure::new(
                    "MODEL_PORT_START_FAILED",
                    format!("model stream callback returned {status:?}"),
                ));
            }
            let reader = receiver
                .await
                .map_err(|_| {
                    ModelPortFailure::new(
                        "MODEL_PORT_START_FAILED",
                        "model stream callback closed before returning a stream",
                    )
                })?
                .map_err(|message| ModelPortFailure::new("MODEL_PORT_START_FAILED", message))?;
            Ok(Box::pin(NativeModelStream {
                request_id: request.request_id,
                reader,
                cancel_callback,
                finished: false,
            }) as ModelPortStream)
        })
    }
}

#[module_init]
fn initialize_native_runtime() {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_name("winwincode-kernel")
        .thread_stack_size(CODEX_TOKIO_WORKER_STACK_BYTES)
        .build()
        .unwrap_or_else(|error| panic!("failed to create WinWinCode kernel runtime: {error}"));
    napi::bindgen_prelude::create_custom_tokio_runtime(runtime);
}

/// JavaScript construction options.
#[napi(object)]
pub struct NativeKernelOptions {
    /// Absolute `WinWinCode` data directory.
    pub home: String,
    /// Bundled helper executable used for sandbox and filesystem child operations.
    pub helper_executable: String,
    /// Per-session ordered event capacity.
    pub event_capacity: Option<u32>,
    /// Graceful shutdown deadline.
    pub shutdown_timeout_millis: Option<u32>,
    /// Optional bundled Linux sandbox helper.
    pub linux_sandbox_executable: Option<String>,
}

/// JavaScript session options.
#[napi(object)]
pub struct NativeSessionOptions {
    /// Absolute workspace directory.
    pub cwd: String,
    /// Exact DSH provider route.
    pub provider: String,
    /// Exact model identifier within the provider route.
    pub model: String,
    /// Strict `StrongFlow` role authority JSON. Omitted for ordinary DSH chat sessions.
    pub governed_authority_json: Option<String>,
}

/// JavaScript resume options.
#[napi(object)]
pub struct NativeResumeOptions {
    /// Absolute rollout file path.
    pub rollout_path: String,
    /// Absolute workspace directory.
    pub cwd: String,
    /// Exact DSH provider route.
    pub provider: String,
    /// Exact model identifier within the provider route.
    pub model: String,
    /// Strict `StrongFlow` role authority JSON. Omitted for ordinary DSH chat sessions.
    pub governed_authority_json: Option<String>,
}

/// JavaScript fork options.
#[napi(object)]
pub struct NativeForkOptions {
    /// Source live session.
    pub source_session_id: String,
    /// Optional replacement workspace directory.
    pub cwd: Option<String>,
    /// Optional replacement DSH provider route.
    pub provider: Option<String>,
    /// Optional replacement model identifier.
    pub model: Option<String>,
}

/// JavaScript steering options.
#[napi(object)]
pub struct NativeSteerOptions {
    /// Live session.
    pub session_id: String,
    /// Turn that must still be active.
    pub expected_turn_id: String,
    /// Steering text.
    pub text: String,
}

/// JavaScript approval response tied to one source callback.
#[napi(object)]
pub struct NativeApprovalResponse {
    /// Live session that emitted the request.
    pub session_id: String,
    /// `exec` or `patch`.
    pub kind: String,
    /// Effective Codex approval identity.
    pub operation_id: String,
    /// Source turn for command approvals, when present.
    pub turn_id: Option<String>,
    /// `approved`, `approved_for_session`, `denied`, or `abort`.
    pub decision: String,
    /// Required when `decision` is `denied`.
    pub rejection: Option<String>,
}

/// JavaScript response for one suspended `StrongFlow` dynamic-tool call.
#[napi(object)]
pub struct NativeDynamicToolResponse {
    pub session_id: String,
    pub call_id: String,
    pub success: bool,
    pub text: String,
}

/// Build identity returned to the host.
#[napi(object)]
pub struct NativeBuildInfo {
    pub interface_version: u32,
    pub codex_tag: String,
    pub codex_commit: String,
    pub patch_set: Vec<String>,
    pub event_capacity: u32,
}

/// Session identity returned to the host.
#[napi(object)]
pub struct NativeSessionInfo {
    pub session_id: String,
    pub rollout_path: Option<String>,
    pub effective_policy_json: Option<String>,
}

/// Ordered event returned to the host.
#[napi(object)]
pub struct NativeEvent {
    pub sequence: String,
    pub kind: String,
    pub payload_json: String,
}

/// Result of polling one ordered event stream.
#[napi(object)]
pub struct NativeEventPoll {
    /// `event`, `timeout`, or `closed`.
    pub status: String,
    /// Present only when `status` is `event`.
    pub event: Option<NativeEvent>,
}

/// Turn submission result returned to the host.
#[napi(object)]
pub struct NativeSubmissionInfo {
    pub status: String,
    pub turn_id: Option<String>,
    pub reason: Option<String>,
}

/// Shutdown result returned to the host.
#[napi(object)]
pub struct NativeShutdownInfo {
    pub completed: Vec<String>,
    pub submit_failed: Vec<String>,
    pub timed_out: Vec<String>,
}

/// One process-local Codex kernel. Every method is panic-contained by the core boundary.
#[napi]
pub struct NativeKernel {
    kernel: Arc<Kernel>,
}

#[napi]
impl NativeKernel {
    /// Construct a lazy kernel. Codex background services start on first session use.
    ///
    /// # Errors
    ///
    /// Returns a JavaScript error when the kernel options are invalid or its home cannot be
    /// created.
    #[allow(clippy::needless_pass_by_value)]
    #[napi(constructor)]
    pub fn new(
        options: NativeKernelOptions,
        model_stream: Function<'_, FnArgs<(String,)>, ReadableStream<'static, String>>,
        model_cancel: Function<'_, FnArgs<(String,)>, ()>,
    ) -> Result<Self> {
        let mut kernel_options = KernelOptions::new(
            PathBuf::from(options.home),
            PathBuf::from(options.helper_executable),
        );
        if let Some(capacity) = options.event_capacity {
            kernel_options.event_capacity = usize::try_from(capacity)
                .map_err(|error| Error::new(Status::InvalidArg, error.to_string()))?;
        }
        if let Some(timeout) = options.shutdown_timeout_millis {
            kernel_options.shutdown_timeout = Duration::from_millis(u64::from(timeout));
        }
        kernel_options.linux_sandbox_executable =
            options.linux_sandbox_executable.map(PathBuf::from);
        let stream_callback = model_stream
            .build_threadsafe_function::<String>()
            .callee_handled::<false>()
            .weak::<true>()
            .build_callback(|context| Ok(FnArgs::from((context.value,))))?;
        let cancel_callback = model_cancel
            .build_threadsafe_function::<String>()
            .callee_handled::<false>()
            .weak::<true>()
            .build_callback(|context| Ok(FnArgs::from((context.value,))))?;
        let model_port = Arc::new(NativeModelPort {
            stream_callback: Arc::new(stream_callback),
            cancel_callback: Arc::new(cancel_callback),
        });
        let kernel =
            Kernel::new(kernel_options, model_port).map_err(|error| to_napi_error(&error))?;
        Ok(Self {
            kernel: Arc::new(kernel),
        })
    }

    /// Return the exact Codex source and applied patch set.
    #[napi]
    pub fn build_info(&self) -> NativeBuildInfo {
        self.kernel.build_info().into()
    }

    /// Create a fresh Codex session.
    ///
    /// # Errors
    ///
    /// Returns a typed native error when the kernel cannot create the session.
    #[napi]
    pub async fn create_session(&self, options: NativeSessionOptions) -> Result<NativeSessionInfo> {
        let options = kernel_session_options(
            options.cwd,
            options.provider,
            options.model,
            options.governed_authority_json,
        )?;
        self.kernel
            .create_session(options)
            .await
            .map(Into::into)
            .map_err(|error| to_napi_error(&error))
    }

    /// Resume a persisted Codex session.
    ///
    /// # Errors
    ///
    /// Returns a typed native error when the kernel cannot resume the rollout.
    #[napi]
    pub async fn resume_session(&self, options: NativeResumeOptions) -> Result<NativeSessionInfo> {
        let rollout_path = PathBuf::from(options.rollout_path);
        let session_options = kernel_session_options(
            options.cwd,
            options.provider,
            options.model,
            options.governed_authority_json,
        )?;
        self.kernel
            .resume_session(rollout_path, session_options)
            .await
            .map(Into::into)
            .map_err(|error| to_napi_error(&error))
    }

    /// Fork a live Codex session.
    ///
    /// # Errors
    ///
    /// Returns a typed native error when the source session cannot be forked.
    #[napi]
    pub async fn fork_session(&self, options: NativeForkOptions) -> Result<NativeSessionInfo> {
        let fork_options = KernelForkOptions {
            cwd: options.cwd.map(PathBuf::from),
            provider: options.provider,
            model: options.model,
        };
        self.kernel
            .fork_session(&options.source_session_id, fork_options)
            .await
            .map(Into::into)
            .map_err(|error| to_napi_error(&error))
    }

    /// Start a turn or steer the active turn through the same Codex session.
    ///
    /// # Errors
    ///
    /// Returns a typed native error when the input is rejected or the session is unavailable.
    #[napi]
    pub async fn submit_turn(
        &self,
        session_id: String,
        text: String,
    ) -> Result<NativeSubmissionInfo> {
        self.kernel
            .submit_turn(&session_id, text)
            .await
            .map(Into::into)
            .map_err(|error| to_napi_error(&error))
    }

    /// Steer only the expected active turn.
    ///
    /// # Errors
    ///
    /// Returns a typed native error when the input is rejected or the expected turn is stale.
    #[napi]
    pub async fn steer(&self, options: NativeSteerOptions) -> Result<NativeSubmissionInfo> {
        self.kernel
            .steer(&options.session_id, options.expected_turn_id, options.text)
            .await
            .map(Into::into)
            .map_err(|error| to_napi_error(&error))
    }

    /// Interrupt an active turn without closing its session.
    ///
    /// # Errors
    ///
    /// Returns a typed native error when the session is unavailable or cannot be interrupted.
    #[napi]
    pub async fn interrupt(&self, session_id: String) -> Result<String> {
        self.kernel
            .interrupt(&session_id)
            .await
            .map_err(|error| to_napi_error(&error))
    }

    /// Resolve one pending command or patch approval by the identity from its source event.
    ///
    /// # Errors
    ///
    /// Returns a typed native error for an invalid response or rejected Codex submission.
    #[napi]
    pub async fn resolve_approval(&self, response: NativeApprovalResponse) -> Result<String> {
        let response = kernel_approval_response(response)?;
        self.kernel
            .resolve_approval(response)
            .await
            .map_err(|error| to_napi_error(&error))
    }

    /// Resolve one pending `StrongFlow` dynamic-tool call by its source identity.
    ///
    /// # Errors
    ///
    /// Returns a typed native error for invalid identities or a rejected Codex submission.
    #[napi]
    pub async fn resolve_dynamic_tool(
        &self,
        response: NativeDynamicToolResponse,
    ) -> Result<String> {
        self.kernel
            .resolve_dynamic_tool(KernelDynamicToolCallResponse {
                session_id: response.session_id,
                call_id: response.call_id,
                success: response.success,
                text: response.text,
            })
            .await
            .map_err(|error| to_napi_error(&error))
    }

    /// Read one ordered event and distinguish timeout from stream closure.
    ///
    /// # Errors
    ///
    /// Returns a typed native error when the session is unavailable.
    #[napi]
    pub async fn next_event(
        &self,
        session_id: String,
        timeout_millis: Option<u32>,
    ) -> Result<NativeEventPoll> {
        self.kernel
            .next_event(
                &session_id,
                timeout_millis.map(|value| Duration::from_millis(u64::from(value))),
            )
            .await
            .map(Into::into)
            .map_err(|error| to_napi_error(&error))
    }

    /// List live sessions in stable order.
    ///
    /// # Errors
    ///
    /// Returns a typed native error when the kernel is closed or cannot initialize.
    #[napi]
    pub async fn list_sessions(&self) -> Result<Vec<String>> {
        self.kernel
            .list_sessions()
            .await
            .map_err(|error| to_napi_error(&error))
    }

    /// Close and unregister one session.
    ///
    /// # Errors
    ///
    /// Returns a typed native error when the session is unknown or cannot shut down cleanly.
    #[napi]
    pub async fn close_session(&self, session_id: String) -> Result<()> {
        self.kernel
            .close_session(&session_id)
            .await
            .map_err(|error| to_napi_error(&error))
    }

    /// Shut down every session and reject later work.
    ///
    /// # Errors
    ///
    /// Returns a typed native error if the embedded shutdown path fails.
    #[napi]
    pub async fn shutdown(&self) -> Result<NativeShutdownInfo> {
        self.kernel
            .shutdown()
            .await
            .map(Into::into)
            .map_err(|error| to_napi_error(&error))
    }
}

impl Drop for NativeKernel {
    fn drop(&mut self) {
        if let Some(shutdown) = self.kernel.best_effort_shutdown() {
            // A JavaScript environment may finalize this object from a thread that is not entered
            // into Tokio. NAPI-RS owns the runtime handle and can schedule from that thread. During
            // final environment teardown even that runtime may already be gone, so finalization
            // must contain the synchronous scheduling panic instead of aborting Node.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                drop(napi_spawn(shutdown));
            }));
        }
    }
}

impl From<KernelBuildInfo> for NativeBuildInfo {
    fn from(info: KernelBuildInfo) -> Self {
        Self {
            interface_version: info.interface_version,
            codex_tag: info.codex_tag.to_string(),
            codex_commit: info.codex_commit.to_string(),
            patch_set: info.patch_set.into_iter().map(str::to_string).collect(),
            event_capacity: info.event_capacity,
        }
    }
}

impl From<SessionInfo> for NativeSessionInfo {
    fn from(info: SessionInfo) -> Self {
        Self {
            session_id: info.session_id,
            rollout_path: info.rollout_path,
            effective_policy_json: info.effective_policy_json,
        }
    }
}

impl From<KernelEvent> for NativeEvent {
    fn from(event: KernelEvent) -> Self {
        Self {
            sequence: event.sequence.to_string(),
            kind: event.kind,
            payload_json: event.payload_json,
        }
    }
}

impl From<EventPoll> for NativeEventPoll {
    fn from(poll: EventPoll) -> Self {
        match poll {
            EventPoll::Event(event) => Self {
                status: "event".to_string(),
                event: Some(event.into()),
            },
            EventPoll::Timeout => Self {
                status: "timeout".to_string(),
                event: None,
            },
            EventPoll::Closed => Self {
                status: "closed".to_string(),
                event: None,
            },
        }
    }
}

impl From<SubmissionInfo> for NativeSubmissionInfo {
    fn from(info: SubmissionInfo) -> Self {
        Self {
            status: info.status.to_string(),
            turn_id: info.turn_id,
            reason: info.reason,
        }
    }
}

impl From<ShutdownInfo> for NativeShutdownInfo {
    fn from(info: ShutdownInfo) -> Self {
        Self {
            completed: info.completed,
            submit_failed: info.submit_failed,
            timed_out: info.timed_out,
        }
    }
}

fn to_napi_error(error: &KernelFailure) -> Error {
    Error::new(
        Status::GenericFailure,
        format!("{ERROR_PREFIX}|{}|{}", error.code(), error.message()),
    )
}

fn kernel_session_options(
    cwd: String,
    provider: String,
    model: String,
    governed_authority_json: Option<String>,
) -> Result<SessionOptions> {
    let governed_authority = governed_authority_json
        .map(|value| winwincode_kernel::GovernedSessionAuthority::from_json(&value))
        .transpose()
        .map_err(|error| to_napi_error(&error))?;
    Ok(SessionOptions {
        cwd: PathBuf::from(cwd),
        provider,
        model,
        governed_authority,
    })
}

fn kernel_approval_response(response: NativeApprovalResponse) -> Result<KernelApprovalResponse> {
    let kind = match response.kind.as_str() {
        "exec" => KernelApprovalKind::Exec,
        "patch" => KernelApprovalKind::Patch,
        _ => {
            return Err(Error::new(
                Status::InvalidArg,
                "approval kind must be exec or patch".to_string(),
            ));
        }
    };
    let decision = match response.decision.as_str() {
        "approved" => KernelApprovalDecision::Approved,
        "approved_for_session" => KernelApprovalDecision::ApprovedForSession,
        "denied" => KernelApprovalDecision::Denied {
            rejection: response.rejection.unwrap_or_default(),
        },
        "abort" => KernelApprovalDecision::Abort,
        _ => {
            return Err(Error::new(
                Status::InvalidArg,
                "approval decision is invalid".to_string(),
            ));
        }
    };
    Ok(KernelApprovalResponse {
        session_id: response.session_id,
        kind,
        operation_id: response.operation_id,
        turn_id: response.turn_id,
        decision,
    })
}

/// Return the embedded kernel descriptor through the native ownership layer.
#[must_use]
pub const fn kernel_descriptor() -> winwincode_kernel::KernelDescriptor {
    winwincode_kernel::descriptor()
}

#[cfg(test)]
mod tests {
    use super::ERROR_PREFIX;
    use super::NativeApprovalResponse;
    use super::kernel_approval_response;
    use super::kernel_descriptor;
    use winwincode_kernel::ApprovalDecision;
    use winwincode_kernel::ApprovalKind;

    #[test]
    fn forwards_the_single_kernel_identity() {
        let descriptor = kernel_descriptor();
        assert_eq!(descriptor.name, "codex-core");
        assert_eq!(descriptor.execution_authorities, 1);
    }

    #[test]
    fn reserves_a_stable_error_prefix() {
        assert_eq!(ERROR_PREFIX, "WINWINCODE_KERNEL_ERROR");
    }

    #[test]
    fn maps_the_exact_native_approval_callback_identity() {
        let response = kernel_approval_response(NativeApprovalResponse {
            session_id: "session-1".to_string(),
            kind: "exec".to_string(),
            operation_id: "approval-1".to_string(),
            turn_id: Some("turn-1".to_string()),
            decision: "denied".to_string(),
            rejection: Some("human rejected".to_string()),
        })
        .expect("valid approval response");
        assert_eq!(response.session_id, "session-1");
        assert_eq!(response.kind, ApprovalKind::Exec);
        assert_eq!(response.operation_id, "approval-1");
        assert_eq!(response.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(
            response.decision,
            ApprovalDecision::Denied {
                rejection: "human rejected".to_string(),
            }
        );
    }
}
