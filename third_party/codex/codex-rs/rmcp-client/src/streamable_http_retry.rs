use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::anyhow;
use codex_exec_server::ExecServerError;
use http::StatusCode;
use rmcp::service::RoleClient;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpError;
use tokio::time;
use tracing::warn;

use crate::elicitation_client_service::ElicitationClientService;
use crate::http_client_adapter::StreamableHttpClientAdapterError;
use crate::oauth::OAuthRuntime;

use super::InitializeContext;
use super::PendingTransport;
use super::RmcpClient;

const JSON_RPC_INTERNAL_ERROR_CODE: i64 = -32603;

impl RmcpClient {
    pub(super) async fn connect_pending_transport_with_initialize_retries(
        &self,
        initial_transport: PendingTransport,
        initialize_context: &InitializeContext,
    ) -> Result<(
        Arc<RunningService<RoleClient, ElicitationClientService>>,
        Option<OAuthRuntime>,
    )> {
        let timeout = initialize_context.timeout;
        let should_retry = match &initial_transport {
            PendingTransport::InProcess { .. } | PendingTransport::Stdio { .. } => false,
            PendingTransport::StreamableHttp { .. }
            | PendingTransport::StreamableHttpWithOAuth { .. }
            | PendingTransport::StreamableHttpWithAccessTokenOnly { .. } => true,
        };
        let retry_deadline = timeout.map(|duration| Instant::now() + duration);
        let mut pending_transport = Some(initial_transport);

        let mut attempt = 1_u32;
        let mut connections = 0_u32;
        loop {
            let transport = match pending_transport.take() {
                Some(transport) => transport,
                None => {
                    let remaining = remaining_initialize_timeout(timeout, retry_deadline)?;
                    match remaining {
                        Some(remaining) => time::timeout(
                            remaining,
                            Self::create_pending_transport(&self.transport_recipe),
                        )
                        .await
                        .map_err(|_| initialize_timeout_error(timeout, remaining))??,
                        None => Self::create_pending_transport(&self.transport_recipe).await?,
                    }
                }
            };
            if let PendingTransport::StreamableHttpWithOAuth { oauth_runtime, .. } = &transport {
                // Credential persistence is retained by its owned refresh task. The
                // caller's total handshake deadline is never extended by refresh.
                oauth_runtime.refresh_if_needed().await?;
            }
            let attempt_timeout = remaining_initialize_timeout(timeout, retry_deadline)?;

            match self
                .connect_pending_transport(transport, initialize_context, attempt_timeout)
                .await
            {
                Ok(result) => return Ok(result),
                Err(error) if should_retry && Self::is_retryable_initialize_error(&error) => {
                    let facts = initialize_retry_facts(&error);
                    if facts.transport.is_some_and(|failure| failure.not_sent) {
                        connections = connections.saturating_add(1);
                    } else {
                        connections = 0;
                    }
                    let Some(delay) =
                        crate::network_retry_policy::retry_delay(attempt, connections, &facts)
                    else {
                        return Err(error);
                    };
                    if connections == 0 {
                        attempt = attempt.saturating_add(1);
                    }
                    warn!(
                        attempt,
                        delay_ms = delay.as_millis(),
                        "streamable HTTP MCP initialize retry scheduled"
                    );
                    if !sleep_with_retry_deadline(delay, retry_deadline).await {
                        let duration = timeout.unwrap_or(delay);
                        return Err(anyhow!(
                            "timed out handshaking with MCP server after {duration:?}"
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn is_retryable_initialize_error(error: &anyhow::Error) -> bool {
        error.chain().any(|source| {
            source
                .downcast_ref::<HandshakeError>()
                .is_some_and(|error| Self::is_retryable_client_initialize_error(&error.source))
                || source
                    .downcast_ref::<rmcp::service::ClientInitializeError>()
                    .is_some_and(Self::is_retryable_client_initialize_error)
        })
    }

    fn is_retryable_client_initialize_error(error: &rmcp::service::ClientInitializeError) -> bool {
        match error {
            rmcp::service::ClientInitializeError::LegacyFallbackFailed { fallback, .. } => {
                Self::is_retryable_client_initialize_error(fallback)
            }
            rmcp::service::ClientInitializeError::TransportError { error, context }
                if matches!(
                    context.as_ref(),
                    "send initialize request" | "send discover request"
                ) =>
            {
                error
                    .error
                    .downcast_ref::<StreamableHttpError<StreamableHttpClientAdapterError>>()
                    .is_some_and(Self::is_retryable_streamable_http_error)
            }
            rmcp::service::ClientInitializeError::TransportError { error, context }
                if context.as_ref() == "send initialized notification" =>
            {
                error
                    .error
                    .downcast_ref::<StreamableHttpError<StreamableHttpClientAdapterError>>()
                    .is_some_and(|error| {
                        matches!(error, StreamableHttpError::TransportChannelClosed)
                            || Self::is_retryable_streamable_http_error(error)
                    })
            }
            _ => false,
        }
    }

    pub(crate) fn is_retryable_streamable_http_error(
        error: &StreamableHttpError<StreamableHttpClientAdapterError>,
    ) -> bool {
        match error {
            StreamableHttpError::Client(StreamableHttpClientAdapterError::NetworkStatus {
                status,
                ..
            }) => matches!(*status, 408 | 425 | 429 | 500..=599),
            StreamableHttpError::Client(StreamableHttpClientAdapterError::HttpRequest(
                ExecServerError::NetworkHttpRequest(failure),
            )) => matches!(
                failure.kind,
                codex_exec_server::HttpNetworkErrorKind::ConnectionUnavailable
                    | codex_exec_server::HttpNetworkErrorKind::TransportInterrupted
                    | codex_exec_server::HttpNetworkErrorKind::Timeout
            ),
            StreamableHttpError::Client(StreamableHttpClientAdapterError::HttpRequest(
                ExecServerError::HttpRequest(_),
            )) => true,
            StreamableHttpError::Client(StreamableHttpClientAdapterError::HttpRequest(
                ExecServerError::Server { code, message },
            )) => {
                *code == JSON_RPC_INTERNAL_ERROR_CODE && message.starts_with("http/request failed:")
            }
            StreamableHttpError::Client(StreamableHttpClientAdapterError::HttpRequest(
                ExecServerError::Protocol(message),
            )) => message.starts_with("http response stream `") && message.contains("` failed:"),
            StreamableHttpError::UnexpectedServerResponse(message) => {
                is_retryable_unexpected_server_response(message.as_ref())
            }
            StreamableHttpError::AuthRequired(_)
            | StreamableHttpError::InsufficientScope(_)
            | StreamableHttpError::SessionExpired
            | StreamableHttpError::UnexpectedContentType(_)
            | StreamableHttpError::ServerDoesNotSupportSse
            | StreamableHttpError::Deserialize(_)
            | StreamableHttpError::Client(StreamableHttpClientAdapterError::SessionExpired404)
            | StreamableHttpError::Client(StreamableHttpClientAdapterError::Header(_)) => false,
            _ => false,
        }
    }
}

fn is_retryable_unexpected_server_response(message: &str) -> bool {
    let Some(message) = message.strip_prefix("HTTP ") else {
        return false;
    };
    let status_code = message
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    let Ok(status) = status_code.parse::<u16>() else {
        return false;
    };
    let Ok(status) = StatusCode::from_u16(status) else {
        return false;
    };
    is_retryable_http_status(status)
}

fn is_retryable_http_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429 | 500..=599)
}

pub(crate) fn streamable_retry_facts(
    error: &StreamableHttpError<StreamableHttpClientAdapterError>,
) -> crate::NetworkRetryFacts {
    match error {
        StreamableHttpError::Client(StreamableHttpClientAdapterError::NetworkStatus {
            status,
            retry_after,
        }) => crate::NetworkRetryFacts {
            status: Some(*status),
            retry_after: retry_after.clone(),
            transport: None,
            reconcile_first: false,
        },
        StreamableHttpError::Client(StreamableHttpClientAdapterError::HttpRequest(
            ExecServerError::NetworkHttpRequest(failure),
        )) => crate::NetworkRetryFacts {
            transport: Some(*failure),
            ..Default::default()
        },
        _ => crate::NetworkRetryFacts::default(),
    }
}
fn initialize_retry_facts(error: &anyhow::Error) -> crate::NetworkRetryFacts {
    fn facts(error: &rmcp::service::ClientInitializeError) -> crate::NetworkRetryFacts {
        match error {
            rmcp::service::ClientInitializeError::LegacyFallbackFailed { fallback, .. } => {
                facts(fallback)
            }
            rmcp::service::ClientInitializeError::TransportError { error, .. } => error
                .error
                .downcast_ref::<StreamableHttpError<StreamableHttpClientAdapterError>>()
                .map(streamable_retry_facts)
                .unwrap_or_default(),
            _ => Default::default(),
        }
    }
    for source in error.chain() {
        if let Some(error) = source.downcast_ref::<HandshakeError>() {
            return facts(&error.source);
        }
        if let Some(error) = source.downcast_ref::<rmcp::service::ClientInitializeError>() {
            return facts(error);
        }
    }
    Default::default()
}

fn remaining_initialize_timeout(
    timeout: Option<Duration>,
    deadline: Option<Instant>,
) -> Result<Option<Duration>> {
    let Some(deadline) = deadline else {
        return Ok(None);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(initialize_timeout_error(timeout, remaining))
    } else {
        Ok(Some(remaining))
    }
}

fn initialize_timeout_error(timeout: Option<Duration>, fallback: Duration) -> anyhow::Error {
    let duration = timeout.unwrap_or(fallback);
    anyhow!("timed out handshaking with MCP server after {duration:?}")
}

pub(crate) async fn sleep_with_retry_deadline(delay: Duration, deadline: Option<Instant>) -> bool {
    if let Some(deadline) = deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        time::timeout(remaining, time::sleep(delay)).await.is_ok()
    } else {
        time::sleep(delay).await;
        true
    }
}

#[derive(Debug, thiserror::Error)]
#[error("handshaking with MCP server failed: {source}")]
pub(super) struct HandshakeError {
    #[source]
    pub(super) source: rmcp::service::ClientInitializeError,
}

#[cfg(test)]
#[path = "streamable_http_retry_tests.rs"]
mod tests;
