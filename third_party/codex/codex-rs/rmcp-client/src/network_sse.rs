//! One retry owner for MCP GET streams and Last-Event-ID continuations.
use std::sync::Arc;
use std::time::Duration;

use futures::{
    StreamExt,
    future::BoxFuture,
    stream::{self, BoxStream},
};
use rmcp::transport::streamable_http_client::StreamableHttpError;
use sse_stream::{Error, Sse};

use crate::http_client_adapter::StreamableHttpClientAdapterError;
use crate::network_retry_policy::{NetworkRetryFacts, retry_delay};
use crate::rmcp_client::streamable_http_retry::streamable_retry_facts;

type Events = BoxStream<'static, Result<Sse, Error>>;
type OpenError = StreamableHttpError<StreamableHttpClientAdapterError>;
pub(crate) type OpenStream =
    Arc<dyn Fn(Option<String>) -> BoxFuture<'static, Result<Events, OpenError>> + Send + Sync>;

#[derive(Default)]
pub(crate) struct RetryState {
    attempt: u32,
    connections: u32,
}
impl RetryState {
    fn delay(&mut self, facts: &NetworkRetryFacts) -> Option<Duration> {
        self.attempt = self.attempt.max(1);
        if facts.transport.is_some_and(|failure| failure.not_sent) {
            self.connections = self.connections.saturating_add(1);
        } else {
            self.connections = 0;
        }
        let delay = retry_delay(self.attempt, self.connections, facts);
        if self.connections == 0 {
            self.attempt = self.attempt.saturating_add(1);
        }
        delay
    }
}

pub(crate) async fn open(
    connect: &OpenStream,
    id: Option<String>,
    retry: &mut RetryState,
) -> Result<Events, OpenError> {
    loop {
        match connect(id.clone()).await {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                // Permanent protocol/auth/TLS errors never enter a generic reconnect loop.
                if !crate::rmcp_client::RmcpClient::is_retryable_streamable_http_error(&error) {
                    return Err(error);
                }
                let Some(delay) = retry.delay(&streamable_retry_facts(&error)) else {
                    return Err(error);
                };
                tokio::time::sleep(delay).await;
            }
        }
    }
}

pub(crate) fn resume(
    events: Events,
    connect: OpenStream,
    retry: RetryState,
    id: Option<String>,
    allow_without_id: bool,
    on_stop: Arc<dyn Fn(Option<String>) + Send + Sync>,
) -> Events {
    stream::unfold(
        Some((events, connect, retry, id, None::<Duration>)),
        move |state| {
            let on_stop = on_stop.clone();
            async move {
                let (mut events, connect, mut retry, mut id, mut server_delay) = state?;
                loop {
                    let failure = match events.next().await {
                        Some(Ok(event)) => {
                            if let Some(value) = &event.id {
                                id = Some(value.clone());
                            }
                            if let Some(value) = event.retry {
                                server_delay = Some(Duration::from_millis(value));
                            }
                            retry = RetryState::default(); // Received progress starts a new reconnect budget.
                            return Some((
                                Ok(event),
                                Some((events, connect, retry, id, server_delay)),
                            ));
                        }
                        Some(Err(error)) => {
                            if is_invalid_data(&error) || (!allow_without_id && id.is_none()) {
                                on_stop(id.clone());
                                return Some((Err(error), None));
                            }
                            error
                        }
                        None if !allow_without_id && id.is_none() => return None,
                        None => Error::Body(Box::new(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "MCP SSE ended",
                        ))),
                    };
                    let Some(delay) = retry.delay(&NetworkRetryFacts::default()) else {
                        on_stop(id.clone());
                        return Some((Err(failure), None));
                    };
                    // The server's SSE retry field is a lower bound, never shortened.
                    tokio::time::sleep(delay.max(server_delay.take().unwrap_or_default())).await;
                    match open(&connect, id.clone(), &mut retry).await {
                        Ok(stream) => events = stream,
                        Err(_) => {
                            // RMCP makes one immediate reconnect before consulting its retry
                            // policy. Fence that continuation before yielding the terminal error.
                            on_stop(id.clone());
                            return Some((
                                Err(Error::Body(Box::new(std::io::Error::other(
                                    "MCP SSE continuation failed",
                                )))),
                                None,
                            ));
                        }
                    }
                }
            }
        },
    )
    .boxed()
}

fn is_invalid_data(error: &Error) -> bool {
    if !matches!(error, Error::Body(_)) {
        return true;
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = source {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::InvalidData)
        {
            return true;
        }
        source = error.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_retry_policy::{NetworkRetryPolicy, TEST_POLICY};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    fn policy() -> Arc<NetworkRetryPolicy> {
        Arc::new(|attempt, _, facts| {
            assert!(!facts.reconcile_first);
            (attempt < 4).then_some(Duration::from_millis(1))
        })
    }
    fn event(id: &str) -> Sse {
        Sse {
            id: Some(id.to_owned()),
            data: Some("{}".to_owned()),
            ..Default::default()
        }
    }
    #[tokio::test]
    async fn sse_continuation_uses_last_event_id_and_one_typed_budget() {
        TEST_POLICY
            .scope(policy(), async {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let observed = calls.clone();
                let connect: OpenStream = Arc::new(move |id| {
                    let index = {
                        let mut calls = observed.lock().unwrap();
                        calls.push(id);
                        calls.len()
                    };
                    Box::pin(async move {
                        if index == 1 {
                            Err(StreamableHttpError::Client(
                                StreamableHttpClientAdapterError::NetworkStatus {
                                    status: 503,
                                    retry_after: Some("0".to_owned()),
                                },
                            ))
                        } else {
                            Ok(stream::iter([Ok(event("2"))])
                                .chain(stream::pending())
                                .boxed())
                        }
                    })
                });
                let initial = stream::iter([
                    Ok(event("1")),
                    Err(Error::Body(Box::new(std::io::Error::other("closed")))),
                ])
                .boxed();
                let mut resumed = resume(
                    initial,
                    connect,
                    Default::default(),
                    None,
                    false,
                    Arc::new(|_| {}),
                );
                assert_eq!(
                    resumed.next().await.unwrap().unwrap().id.as_deref(),
                    Some("1")
                );
                assert_eq!(
                    resumed.next().await.unwrap().unwrap().id.as_deref(),
                    Some("2")
                );
                assert_eq!(
                    *calls.lock().unwrap(),
                    vec![Some("1".to_owned()), Some("1".to_owned())]
                );
            })
            .await;
    }
    #[tokio::test]
    async fn sse_open_exhaustion_and_tls_stop_do_not_multiply_attempts() {
        TEST_POLICY
            .scope(policy(), async {
                let calls = Arc::new(AtomicUsize::new(0));
                let observed = calls.clone();
                let connect: OpenStream = Arc::new(move |_| {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async {
                        Err(StreamableHttpError::Client(
                            StreamableHttpClientAdapterError::NetworkStatus {
                                status: 503,
                                retry_after: None,
                            },
                        ))
                    })
                });
                assert!(open(&connect, None, &mut Default::default()).await.is_err());
                assert_eq!(calls.load(Ordering::SeqCst), 4);
                let connect: OpenStream = Arc::new(move |_| {
                    Box::pin(async {
                        Err(StreamableHttpError::Client(
                            StreamableHttpClientAdapterError::HttpRequest(
                                codex_exec_server::ExecServerError::NetworkHttpRequest(
                                    codex_exec_server::HttpNetworkFailure {
                                        kind: codex_exec_server::HttpNetworkErrorKind::TlsInvalid,
                                        not_sent: true,
                                    },
                                ),
                            ),
                        ))
                    })
                });
                assert!(open(&connect, None, &mut Default::default()).await.is_err());
            })
            .await;
    }
    #[tokio::test]
    async fn a_post_without_resume_id_and_invalid_sse_never_reopens() {
        TEST_POLICY
            .scope(policy(), async {
                let connect: OpenStream =
                    Arc::new(|_| panic!("unsafe or invalid stream cannot reopen"));
                let mut resumed = resume(
                    stream::empty().boxed(),
                    connect.clone(),
                    Default::default(),
                    None,
                    false,
                    Arc::new(|_| {}),
                );
                assert!(resumed.next().await.is_none());
                let mut resumed = resume(
                    stream::iter([Err(Error::InvalidLine)]).boxed(),
                    connect,
                    Default::default(),
                    Some("1".to_owned()),
                    true,
                    Arc::new(|_| {}),
                );
                assert!(resumed.next().await.unwrap().is_err());
                assert!(resumed.next().await.is_none());
            })
            .await;
    }
}
