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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPortRequest {
    /// Stable identity shared by cancellation, diagnostics, and every stream message.
    pub request_id: String,
    /// Serialized [`ModelStreamRequest`] using the public host wire contract.
    pub payload_json: String,
}

/// Serializable failure facts retained across the host/native boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPortFailure {
    /// Stable DSH or bridge failure category.
    pub code: String,
    /// Human-readable diagnostic that must not contain provider credentials.
    pub message: String,
    /// Provider HTTP status, when supplied by DSH.
    pub status: Option<u16>,
    /// Provider-requested retry delay, when supplied by DSH.
    pub provider_retry_after_millis: Option<u64>,
    /// Provider-issued request identity, when supplied by DSH.
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
            provider_retry_after_millis: None,
            provider_request_id: None,
        }
    }
}

impl fmt::Display for ModelPortFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "[DSH:{}] {}", self.code, self.message)
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
                    message: format!("[DSH:MODEL_PORT_REQUEST_INVALID] {error}"),
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
        token_usage: Option<TokenUsage>,
        #[serde(rename = "endTurn")]
        end_turn: Option<bool>,
    },
    Error {
        error: ModelPortFailureWire,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelPortFailureWire {
    code: String,
    message: String,
    status: Option<u16>,
    provider_retry_after_millis: Option<u64>,
    provider_request_id: Option<String>,
}

impl From<ModelPortFailureWire> for ModelPortFailure {
    fn from(failure: ModelPortFailureWire) -> Self {
        Self {
            code: failure.code,
            message: failure.message,
            status: failure.status,
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
                token_usage,
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
                            "[DSH:MODEL_PORT_PROTOCOL_INVALID] {error}"
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
            "[DSH:STREAM_CLOSED] model stream ended without a terminal message".to_string(),
        )))
        .await;
}

fn model_port_api_error(failure: &ModelPortFailure) -> ApiError {
    let message = failure.to_string();
    match failure.code.as_str() {
        "CONTEXT_WINDOW_EXCEEDED" => ApiError::ContextWindowExceeded,
        "QUOTA" | "QUOTA_EXCEEDED" => ApiError::QuotaExceeded,
        "RATE_LIMIT" | "SERVER" | "TIMEOUT" | "TRANSPORT" | "EMPTY_RESPONSE" => {
            ApiError::Retryable {
                message,
                delay: failure
                    .provider_retry_after_millis
                    .map(Duration::from_millis),
            }
        }
        "AUTH"
        | "MISSING_CREDENTIAL"
        | "INVALID_CREDENTIAL"
        | "INVALID_REQUEST"
        | "NO_ADAPTER"
        | "UNKNOWN_MODEL"
        | "UNSUPPORTED_CONTENT"
        | "UNSUPPORTED_OPTION"
        | "UNSUPPORTED_REASONING_EFFORT"
        | "UNSUPPORTED_TOOL" => ApiError::InvalidRequest { message },
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
                assert!(message.starts_with("[DSH:RATE_LIMIT]"));
                assert_eq!(delay.map(|value| value.as_millis()), Some(750));
            }
            other => panic!("unexpected error: {other:?}"),
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
