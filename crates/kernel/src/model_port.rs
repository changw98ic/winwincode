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

#[derive(Clone, PartialEq, Eq)]
pub struct ModelPortRequest {
    pub request_id: String,
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

#[derive(Debug, Clone, PartialEq)]
pub struct ModelPortFailure {
    pub code: String,
    pub message: String,
    pub status: Option<u16>,
    pub retryable: Option<bool>,
    pub provider_retry_after_millis: Option<u64>,
    pub provider_request_id: Option<String>,
    /// Original bounded attempt facts; absent on older streams.
    pub facts: Option<Box<winwincode_execution_port::generated::ModelFailureFacts>>,
}

impl ModelPortFailure {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            status: None,
            retryable: None,
            provider_retry_after_millis: None,
            provider_request_id: None,
            facts: None,
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

pub type ModelPortStream =
    Pin<Box<dyn Stream<Item = Result<String, ModelPortFailure>> + Send + 'static>>;

pub trait ModelPort: fmt::Debug + Send + Sync {
    fn compaction_context_overhead_bytes(
        &self,
        _thread_id: &str,
    ) -> Result<usize, ModelPortFailure> {
        Ok(0)
    }

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
    fn compaction_context_overhead_bytes(&self, thread_id: &str) -> Result<usize, ApiError> {
        self.port
            .compaction_context_overhead_bytes(thread_id)
            .map_err(|error| model_port_api_error(&error))
    }

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

#[allow(clippy::struct_field_names)]
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
    facts: Option<winwincode_execution_port::generated::ModelFailureFacts>,
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
            facts: failure.facts.map(Box::new),
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
            Ok(payload) => {
                if let Ok(message) = serde_json::from_str::<ModelPortMessage>(&payload) {
                    message
                } else {
                    let _ = tx_event
                        .send(Err(ApiError::Stream(
                            "[WINWINCODE_KERNEL:MODEL_PORT_PROTOCOL_INVALID] invalid canonical model message".to_owned()
                        )
                        .without_retry()))
                        .await;
                    return;
                }
            }
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
        )
        .without_retry()))
        .await;
}

fn provider_http_error(failure: &ModelPortFailure, status: u16, message: String) -> ApiError {
    let mut headers = http::HeaderMap::new();
    if let Some(value) = failure
        .provider_request_id
        .as_ref()
        .and_then(|value| http::HeaderValue::from_str(value).ok())
    {
        headers.insert("x-request-id", value);
    }
    ApiError::Transport(codex_api::TransportError::Http {
        status: http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_GATEWAY),
        url: None,
        headers: Some(headers),
        body: Some(message),
    })
}

fn model_port_api_error(failure: &ModelPortFailure) -> ApiError {
    let message = failure.to_string();
    let terminal = failure.retryable == Some(false);
    match failure.code.as_str() {
        "CONTEXT_WINDOW_EXCEEDED" => ApiError::ContextWindowExceeded,
        "QUOTA" | "QUOTA_EXCEEDED" => ApiError::QuotaExceeded,
        "AUTH" | "MISSING_CREDENTIAL" | "INVALID_CREDENTIAL" => {
            provider_http_error(failure, failure.status.unwrap_or(401), message).without_retry()
        }
        "LEASE_EXPIRED" => ApiError::ExecutionLeaseExpired,
        "MODEL_AUTHORITY_EXPIRED" => ApiError::ModelAuthorityExpired,
        "STALE_FENCING_TOKEN"
        | "WORKER_INSTANCE_CHANGED"
        | "MODEL_AUTHORITY_STALE"
        | "DEVICE_MODEL_PAUSED"
        | "DEVICE_MODEL_INTERRUPTED"
        | "DEVICE_MODEL_IDENTITY_CONFLICT"
        | "DEVICE_PROVIDER_CREDENTIAL_LEAK_BLOCKED"
        | "CANCELLED" => ApiError::Stream(message).without_retry(),
        "INVALID_REQUEST"
        | "NO_ADAPTER"
        | "UNKNOWN_MODEL"
        | "UNSUPPORTED_CONTENT"
        | "CONTENT_FILTER"
        | "UNSUPPORTED_OPTION"
        | "UNSUPPORTED_REASONING_EFFORT"
        | "UNSUPPORTED_TOOL" => ApiError::InvalidRequest { message },
        _ if terminal => {
            let source = match failure.code.as_str() {
                "RATE_LIMIT" | "SERVER" => provider_http_error(
                    failure,
                    failure.status.unwrap_or(if failure.code == "RATE_LIMIT" {
                        429
                    } else {
                        503
                    }),
                    message,
                ),
                "TIMEOUT" => ApiError::Transport(codex_api::TransportError::Timeout),
                _ => ApiError::Stream(message),
            };
            source.without_retry()
        }
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
    fn host_retry_exhaustion_preserves_source_category_and_suppresses_core_retry() {
        use codex_protocol::error::CodexErrorDetails;
        for code in [
            "RATE_LIMIT",
            "SERVER",
            "TIMEOUT",
            "TRANSPORT",
            "INCOMPLETE_STREAM",
        ] {
            let mut failure = ModelPortFailure::new(code, "host attempts exhausted");
            failure.retryable = Some(false);
            let error = codex_api::map_api_error(model_port_api_error(&failure));
            assert!(
                !error.is_retryable(),
                "{code} cannot acquire another Core retry budget"
            );
            match code {
                "RATE_LIMIT" => assert!(
                    matches!(error.details(), CodexErrorDetails::RetryLimit(details) if details.status.as_u16() == 429)
                ),
                "SERVER" => assert!(
                    matches!(error.details(), CodexErrorDetails::UnexpectedStatus(details) if details.status.as_u16() == 503)
                ),
                "TIMEOUT" => assert!(matches!(error.details(), CodexErrorDetails::RequestTimeout)),
                _ => assert!(matches!(error.details(), CodexErrorDetails::Stream(_))),
            }
            assert!(!matches!(
                error.details(),
                CodexErrorDetails::InvalidRequest(_)
            ));
        }
    }

    #[test]
    fn http_failure_retains_request_identity_and_typed_protocol_category() {
        use codex_protocol::error::CodexErrorDetails;
        use codex_protocol::protocol::CodexErrorInfo;
        for (code, status) in [("RATE_LIMIT", 429), ("SERVER", 503), ("AUTH", 403)] {
            let mut failure = ModelPortFailure::new(code, "safe host summary");
            failure.status = Some(status);
            failure.retryable = Some(false);
            failure.provider_request_id = Some("upstream-42".to_owned());
            let error = codex_api::map_api_error(model_port_api_error(&failure));
            assert!(!error.is_retryable());
            let request_id = match error.details() {
                CodexErrorDetails::RetryLimit(details) => details.request_id.as_deref(),
                CodexErrorDetails::UnexpectedStatus(details) => details.request_id.as_deref(),
                other => panic!("lost HTTP classification: {other:?}"),
            };
            assert_eq!(request_id, Some("upstream-42"));
            assert_eq!(
                error.to_codex_protocol_error(),
                match code {
                    "RATE_LIMIT" => CodexErrorInfo::ResponseTooManyFailedAttempts {
                        http_status_code: Some(429)
                    },
                    "SERVER" => CodexErrorInfo::HttpConnectionFailed {
                        http_status_code: Some(503)
                    },
                    _ => CodexErrorInfo::Unauthorized,
                }
            );
        }
    }

    #[test]
    fn fatal_tool_runtime_and_invalid_request_have_distinct_typed_categories() {
        use codex_protocol::error::CodexErr;
        use codex_protocol::protocol::CodexErrorInfo;
        assert_eq!(
            CodexErr::Fatal("runtime failed".to_owned()).to_codex_protocol_error(),
            CodexErrorInfo::InternalServerError
        );
        assert_eq!(
            CodexErr::InvalidRequest("request invalid".to_owned()).to_codex_protocol_error(),
            CodexErrorInfo::BadRequest
        );
        assert_eq!(
            CodexErr::RequestTimeout
                .without_retry()
                .to_codex_protocol_error(),
            CodexErrorInfo::ResponseStreamConnectionFailed {
                http_status_code: None
            }
        );
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
    fn execution_lease_expiry_preserves_typed_terminal_cause() {
        for (code, info) in [
            ("LEASE_EXPIRED", "execution_lease_expired"),
            ("MODEL_AUTHORITY_EXPIRED", "model_authority_expired"),
        ] {
            for retryable in [None, Some(false), Some(true)] {
                let mut failure = ModelPortFailure::new(code, "original execution expired");
                failure.retryable = retryable;
                let error = codex_api::map_api_error(model_port_api_error(&failure));
                assert!(!error.is_retryable());
                assert_eq!(
                    serde_json::to_value(error.to_codex_protocol_error()).unwrap(),
                    serde_json::json!(info)
                );
            }
        }
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
            let error = codex_api::map_api_error(model_port_api_error(&failure));
            assert!(!error.is_retryable());
            assert!(!matches!(
                error.details(),
                codex_protocol::error::CodexErrorDetails::InvalidRequest(_)
            ));
        }
    }

    #[test]
    fn canonical_error_preserves_typed_attempt_facts() {
        let facts = serde_json::json!({
            "sourceCode": "DEVICE_PROVIDER_TRANSPORT_FAILED",
            "scope": "model_attempt",
            "cause": "timeout",
            "acceptance": "unknown",
            "phase": "response_body",
            "attemptId": "physical_attempt_2",
            "stopReason": "retry_budget_exhausted",
            "networkDiagnosticCode": "timeout",
            "parserDiagnostic": {
                "stage": "body",
                "eventType": "error",
                "fieldPath": "stream"
            }
        });
        let frame = serde_json::json!({
            "type": "error",
            "error": {
                "code": "TIMEOUT",
                "message": "safe host summary",
                "retryable": false,
                "providerRequestId": "upstream-42",
                "facts": facts.clone()
            }
        });
        let message: super::ModelPortMessage = serde_json::from_value(frame).unwrap();
        let failure = message.into_response_event().unwrap_err();
        assert_eq!(failure.code, "TIMEOUT");
        assert_eq!(failure.retryable, Some(false));
        assert_eq!(failure.provider_request_id.as_deref(), Some("upstream-42"));
        assert_eq!(
            serde_json::to_value(failure.facts.as_deref()).unwrap(),
            facts
        );
        let error = codex_api::map_api_error(model_port_api_error(&failure));
        assert!(!error.is_retryable());
        assert!(matches!(
            error.details(),
            codex_protocol::error::CodexErrorDetails::RequestTimeout
        ));
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
            Err(ApiError::HostTerminal { source }) if matches!(*source, ApiError::Transport(codex_api::TransportError::Http { status, .. }) if status.as_u16() == 429)
        ));
    }

    #[tokio::test]
    async fn closed_stream_uses_the_canonical_kernel_error_namespace() {
        let stream: ModelPortStream = Box::pin(futures::stream::empty());
        let (sender, mut receiver) = mpsc::channel(1);
        forward_model_stream(stream, sender).await;

        match receiver.recv().await.expect("terminal stream error") {
            Err(error @ ApiError::HostTerminal { .. }) => {
                let error = codex_api::map_api_error(error);
                assert!(!error.is_retryable());
                assert!(
                    matches!(error.details(), codex_protocol::error::CodexErrorDetails::Stream(message) if message == "[WINWINCODE_KERNEL:STREAM_CLOSED] model stream ended without a terminal message")
                );
            }
            other => panic!("unexpected stream result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_host_frame_stops_core_retry_without_exposing_private_payload() {
        let stream: ModelPortStream = Box::pin(futures::stream::iter([Ok(
            r#"{"type":"private-token-fixture"}"#.to_owned(),
        )]));
        let (sender, mut receiver) = mpsc::channel(1);
        forward_model_stream(stream, sender).await;
        let error = codex_api::map_api_error(receiver.recv().await.unwrap().unwrap_err());
        assert!(!error.is_retryable());
        assert!(!error.to_string().contains("private-token-fixture"));
        assert!(
            matches!(error.details(), codex_protocol::error::CodexErrorDetails::Stream(message) if message.contains("MODEL_PORT_PROTOCOL_INVALID"))
        );
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
