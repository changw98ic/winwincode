//! Provider-neutral model stream boundary owned by the `WinWinCode` host.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use codex_api::ApiError;
use codex_api::ResponseEvent;
use codex_api::ResponseStream;
use codex_core_api::ModelStreamRequest;
use codex_core_api::ModelStreamTransport;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use futures::Stream;
use futures::StreamExt;
use futures::future::BoxFuture;
use serde::Deserialize;
use tokio::sync::mpsc;

const MODEL_STREAM_CHANNEL_CAPACITY: usize = 256;

/// One secret-free request crossing from the Rust kernel to the host model runtime.
#[derive(Clone, PartialEq, Eq)]
pub struct ModelPortRequest {
    /// Stable identity shared by cancellation, diagnostics, and every stream message.
    pub request_id: String,
    /// Serialized [`ModelStreamRequest`] using the public host wire contract.
    pub payload_json: String,
}

impl fmt::Debug for ModelPortRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelPortRequest")
            .field("request_id", &self.request_id)
            .field("payload_json", &"<private>")
            .finish()
    }
}

/// Serializable failure facts retained across the host/native boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPortFailure {
    /// Stable Kernel model-port failure category.
    pub code: String,
    /// Human-readable diagnostic that must not contain provider credentials.
    pub message: String,
    /// Provider HTTP status, when supplied by the model runtime.
    pub status: Option<u16>,
    /// Explicit host retry classification. Absent on legacy canonical errors.
    pub retryable: Option<bool>,
    /// Provider-requested retry delay, when supplied by the model runtime.
    pub provider_retry_after_millis: Option<u64>,
    /// Provider-issued request identity, when supplied by the model runtime.
    pub provider_request_id: Option<String>,
}

impl ModelPortFailure {
    /// Build an owned bridge failure without provider-specific facts.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            status: None,
            retryable: None,
            provider_retry_after_millis: None,
            provider_request_id: None,
        }
    }
}

impl fmt::Display for ModelPortFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "[WINWINCODE_KERNEL:{}] {}",
            self.code, self.message
        )
    }
}

impl std::error::Error for ModelPortFailure {}

/// One ordered stream of JSON messages returned by the host model runtime.
pub type ModelPortStream =
    Pin<Box<dyn Stream<Item = Result<String, ModelPortFailure>> + Send + 'static>>;

/// The kernel's only model-execution dependency.
pub trait ModelPort: fmt::Debug + Send + Sync {
    /// Start one model request. Dropping the returned stream cancels host work.
    fn stream(
        &self,
        request: ModelPortRequest,
    ) -> BoxFuture<'static, Result<ModelPortStream, ModelPortFailure>>;
}

#[derive(Debug)]
pub(crate) struct KernelModelStreamTransport {
    port: Arc<dyn ModelPort>,
}

impl KernelModelStreamTransport {
    pub(crate) fn new(port: Arc<dyn ModelPort>) -> Self {
        Self { port }
    }
}

impl ModelStreamTransport for KernelModelStreamTransport {
    fn stream(
        &self,
        request: ModelStreamRequest,
    ) -> BoxFuture<'static, Result<ResponseStream, ApiError>> {
        let port = Arc::clone(&self.port);
        Box::pin(async move {
            let request_id = request.request_id.clone();
            let payload_json =
                serde_json::to_string(&request).map_err(|error| ApiError::InvalidRequest {
                    message: format!("[WINWINCODE_KERNEL:MODEL_PORT_REQUEST_INVALID] {error}"),
                })?;
            let stream = port
                .stream(ModelPortRequest {
                    request_id: request_id.clone(),
                    payload_json,
                })
                .await
                .map_err(|error| model_port_api_error(&error))?;
            let (tx_event, rx_event) = mpsc::channel(MODEL_STREAM_CHANNEL_CAPACITY);
            tokio::spawn(forward_model_stream(stream, tx_event));
            Ok(ResponseStream {
                rx_event,
                upstream_request_id: Some(request_id),
            })
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ModelPortMessage {
    Created,
    ServerModel {
        model: String,
    },
    OutputItemAdded {
        item: ResponseItem,
    },
    OutputItemDone {
        item: ResponseItem,
    },
    OutputTextDelta {
        delta: String,
    },
    ToolCallInputDelta {
        #[serde(rename = "itemId")]
        item_id: String,
        #[serde(rename = "callId")]
        call_id: Option<String>,
        delta: String,
    },
    ReasoningSummaryDelta {
        delta: String,
        #[serde(rename = "summaryIndex")]
        summary_index: i64,
    },
    ReasoningSummaryDone {
        #[serde(rename = "itemId")]
        item_id: String,
        text: String,
        #[serde(rename = "summaryIndex")]
        summary_index: i64,
    },
    ReasoningContentDelta {
        delta: String,
        #[serde(rename = "contentIndex")]
        content_index: i64,
    },
    ReasoningSummaryPartAdded {
        #[serde(rename = "summaryIndex")]
        summary_index: i64,
    },
    Completed {
        #[serde(rename = "responseId")]
        response_id: String,
        #[serde(rename = "tokenUsage")]
        token_usage: Option<ModelTokenUsageWire>,
        #[serde(rename = "endTurn")]
        end_turn: Option<bool>,
    },
    Error {
        error: ModelPortFailureWire,
    },
}

// The Kernel consumes inclusive counters for context management. Cache details
// may be unavailable; billing retains the original nullable wire fact in Host.
#[allow(clippy::struct_field_names)] // Canonical token field names belong to the wire contract.
#[derive(Debug, Deserialize)]
struct ModelTokenUsageWire {
    input_tokens: i64,
    #[serde(default)]
    cached_input_tokens: Option<i64>,
    #[serde(default)]
    cache_write_input_tokens: i64,
    output_tokens: i64,
    #[serde(default)]
    reasoning_output_tokens: i64,
    total_tokens: i64,
}
impl From<ModelTokenUsageWire> for TokenUsage {
    fn from(value: ModelTokenUsageWire) -> Self {
        Self {
            input_tokens: value.input_tokens,
            cached_input_tokens: value.cached_input_tokens.unwrap_or(0),
            cache_write_input_tokens: value.cache_write_input_tokens,
            output_tokens: value.output_tokens,
            reasoning_output_tokens: value.reasoning_output_tokens,
            total_tokens: value.total_tokens,
            ..Default::default()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelPortFailureWire {
    code: String,
    message: String,
    status: Option<u16>,
    retryable: Option<bool>,
    provider_retry_after_millis: Option<u64>,
    provider_request_id: Option<String>,
}

impl From<ModelPortFailureWire> for ModelPortFailure {
    fn from(failure: ModelPortFailureWire) -> Self {
        Self {
            code: failure.code,
            message: failure.message,
            status: failure.status,
            retryable: failure.retryable,
            provider_retry_after_millis: failure.provider_retry_after_millis,
            provider_request_id: failure.provider_request_id,
        }
    }
}

impl ModelPortMessage {
    fn into_response_event(self) -> Result<ResponseEvent, ModelPortFailure> {
        match self {
            Self::Created => Ok(ResponseEvent::Created),
            Self::ServerModel { model } => Ok(ResponseEvent::ServerModel(model)),
            Self::OutputItemAdded { item } => Ok(ResponseEvent::OutputItemAdded(item)),
            Self::OutputItemDone { item } => Ok(ResponseEvent::OutputItemDone(item)),
            Self::OutputTextDelta { delta } => Ok(ResponseEvent::OutputTextDelta(delta)),
            Self::ToolCallInputDelta {
                item_id,
                call_id,
                delta,
            } => Ok(ResponseEvent::ToolCallInputDelta {
                item_id,
                call_id,
                delta,
            }),
            Self::ReasoningSummaryDelta {
                delta,
                summary_index,
            } => Ok(ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            }),
            Self::ReasoningSummaryDone {
                item_id,
                text,
                summary_index,
            } => Ok(ResponseEvent::ReasoningSummaryDone {
                item_id,
                text,
                summary_index,
            }),
            Self::ReasoningContentDelta {
                delta,
                content_index,
            } => Ok(ResponseEvent::ReasoningContentDelta {
                delta,
                content_index,
            }),
            Self::ReasoningSummaryPartAdded { summary_index } => {
                Ok(ResponseEvent::ReasoningSummaryPartAdded { summary_index })
            }
            Self::Completed {
                response_id,
                token_usage,
                end_turn,
            } => Ok(ResponseEvent::Completed {
                response_id,
                token_usage: token_usage.map(Into::into),
                end_turn,
            }),
            Self::Error { error } => Err(error.into()),
        }
    }

    const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Error { .. })
    }
}

async fn forward_model_stream(
    mut stream: ModelPortStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
) {
    loop {
        let item = tokio::select! {
            () = tx_event.closed() => return,
            item = stream.next() => item,
        };
        let Some(item) = item else {
            break;
        };
        let message = match item {
            Ok(payload) => match serde_json::from_str::<ModelPortMessage>(&payload) {
                Ok(message) => message,
                Err(error) => {
                    let _ = tx_event
                        .send(Err(ApiError::Stream(format!(
                            "[WINWINCODE_KERNEL:MODEL_PORT_PROTOCOL_INVALID] {error}"
                        ))))
                        .await;
                    return;
                }
            },
            Err(error) => {
                let _ = tx_event.send(Err(model_port_api_error(&error))).await;
                return;
            }
        };
        let terminal = message.is_terminal();
        let event = message
            .into_response_event()
            .map_err(|error| model_port_api_error(&error));
        if tx_event.send(event).await.is_err() || terminal {
            return;
        }
    }
    let _ = tx_event
        .send(Err(ApiError::Stream(
            "[WINWINCODE_KERNEL:STREAM_CLOSED] model stream ended without a terminal message"
                .to_string(),
        )))
        .await;
}

fn model_port_api_error(failure: &ModelPortFailure) -> ApiError {
    let message = failure.to_string();
    match failure.code.as_str() {
        "CONTEXT_WINDOW_EXCEEDED" => ApiError::ContextWindowExceeded,
        "QUOTA" | "QUOTA_EXCEEDED" => ApiError::QuotaExceeded,
        "AUTH"
        | "MISSING_CREDENTIAL"
        | "INVALID_CREDENTIAL"
        | "INVALID_REQUEST"
        | "NO_ADAPTER"
        | "UNKNOWN_MODEL"
        | "UNSUPPORTED_CONTENT"
        | "CONTENT_FILTER"
        | "UNSUPPORTED_OPTION"
        | "UNSUPPORTED_REASONING_EFFORT"
        | "UNSUPPORTED_TOOL"
        | "LEASE_EXPIRED"
        | "STALE_FENCING_TOKEN"
        | "WORKER_INSTANCE_CHANGED"
        | "MODEL_AUTHORITY_EXPIRED"
        | "MODEL_AUTHORITY_STALE"
        | "DEVICE_MODEL_PAUSED"
        | "DEVICE_MODEL_INTERRUPTED"
        | "DEVICE_MODEL_IDENTITY_CONFLICT"
        | "DEVICE_PROVIDER_CREDENTIAL_LEAK_BLOCKED"
        | "CANCELLED" => ApiError::InvalidRequest { message },
        _ if failure.retryable == Some(false) => ApiError::InvalidRequest { message },
        "RATE_LIMIT" | "SERVER" | "TIMEOUT" | "TRANSPORT" | "EMPTY_RESPONSE" => {
            ApiError::Retryable {
                message,
                delay: failure
                    .provider_retry_after_millis
                    .map(Duration::from_millis),
            }
        }
        _ if failure.retryable == Some(true) => ApiError::Retryable {
            message,
            delay: failure
                .provider_retry_after_millis
                .map(Duration::from_millis),
        },
        _ => ApiError::Stream(message),
    }
}

#[cfg(test)]
mod tests {
    use super::ModelPortFailure;
    use super::ModelPortStream;
    use super::forward_model_stream;
    use super::model_port_api_error;
    use codex_api::ApiError;
    use futures::Stream;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::task::Context;
    use std::task::Poll;
    use std::time::Duration;
    use tokio::sync::mpsc;

    struct PendingStream {
        dropped: Arc<AtomicBool>,
    }

    impl Stream for PendingStream {
        type Item = Result<String, ModelPortFailure>;

        fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    impl Drop for PendingStream {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn preserves_retry_category_and_delay() {
        let mut failure = ModelPortFailure::new("RATE_LIMIT", "slow down");
        failure.provider_retry_after_millis = Some(750);
        match model_port_api_error(&failure) {
            ApiError::Retryable { message, delay } => {
                assert_eq!(message, "[WINWINCODE_KERNEL:RATE_LIMIT] slow down");
                assert_eq!(delay.map(|value| value.as_millis()), Some(750));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn explicit_terminal_failure_overrides_legacy_retry_category() {
        for code in ["RATE_LIMIT", "SERVER", "TRANSPORT", "INCOMPLETE_STREAM"] {
            let mut failure = ModelPortFailure::new(code, "terminal");
            failure.retryable = Some(false);
            assert!(matches!(
                model_port_api_error(&failure),
                ApiError::InvalidRequest { .. }
            ));
        }
    }

    #[test]
    fn explicit_transient_failure_preserves_retry_after() {
        let mut failure = ModelPortFailure::new("INCOMPLETE_STREAM", "stream interrupted");
        failure.retryable = Some(true);
        failure.provider_retry_after_millis = Some(2_000);
        assert!(
            matches!(model_port_api_error(&failure), ApiError::Retryable { delay: Some(delay), .. } if delay == Duration::from_secs(2))
        );
    }

    #[test]
    fn authority_and_authentication_failures_cannot_request_retry() {
        for code in [
            "AUTH",
            "LEASE_EXPIRED",
            "MODEL_AUTHORITY_EXPIRED",
            "CANCELLED",
            "DEVICE_MODEL_PAUSED",
        ] {
            let mut failure = ModelPortFailure::new(code, "terminal");
            failure.retryable = Some(true);
            assert!(matches!(
                model_port_api_error(&failure),
                ApiError::InvalidRequest { .. }
            ));
        }
    }

    #[tokio::test]
    async fn canonical_error_frame_retains_explicit_terminal_classification() {
        let stream: ModelPortStream = Box::pin(futures::stream::iter([Ok(
            r#"{"type":"error","error":{"code":"RATE_LIMIT","message":"terminal","retryable":false,"status":429,"providerRetryAfterMillis":750}}"#.to_owned(),
        )]));
        let (sender, mut receiver) = mpsc::channel(1);
        forward_model_stream(stream, sender).await;
        assert!(matches!(
            receiver.recv().await.unwrap(),
            Err(ApiError::InvalidRequest { .. })
        ));
    }

    #[tokio::test]
    async fn closed_stream_uses_the_canonical_kernel_error_namespace() {
        let stream: ModelPortStream = Box::pin(futures::stream::empty());
        let (sender, mut receiver) = mpsc::channel(1);
        forward_model_stream(stream, sender).await;

        match receiver.recv().await.expect("terminal stream error") {
            Err(ApiError::Stream(message)) => assert_eq!(
                message,
                "[WINWINCODE_KERNEL:STREAM_CLOSED] model stream ended without a terminal message"
            ),
            other => panic!("unexpected stream result: {other:?}"),
        }
    }

    #[test]
    fn unsupported_capability_is_not_marked_retryable() {
        let failure = ModelPortFailure::new("UNSUPPORTED_OPTION", "schema output");
        assert!(matches!(
            model_port_api_error(&failure),
            ApiError::InvalidRequest { .. }
        ));
    }

    #[test]
    fn content_filter_failure_is_terminal_even_when_host_misclassifies_retry() {
        let mut failure = ModelPortFailure::new("CONTENT_FILTER", "Provider filtered response");
        failure.retryable = Some(true);
        assert!(matches!(
            model_port_api_error(&failure),
            ApiError::InvalidRequest { .. }
        ));
    }

    #[tokio::test]
    async fn dropping_response_receiver_drops_host_stream() {
        let dropped = Arc::new(AtomicBool::new(false));
        let stream: ModelPortStream = Box::pin(PendingStream {
            dropped: Arc::clone(&dropped),
        });
        let (sender, receiver) = mpsc::channel(1);
        let forwarder = tokio::spawn(forward_model_stream(stream, sender));

        tokio::task::yield_now().await;
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(1), forwarder)
            .await
            .expect("forwarder should observe the closed receiver")
            .expect("forwarder task should finish cleanly");

        assert!(dropped.load(Ordering::SeqCst));
    }
}
