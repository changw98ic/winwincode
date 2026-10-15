// SPDX-License-Identifier: Apache-2.0

use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;

use crate::daemon::{ExchangeTransport, ExchangeTransportError};

const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct HttpExchangeTransport {
    endpoint: Mutex<String>,
    io_timeout: Duration,
}

impl HttpExchangeTransport {
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: Mutex::new(endpoint.into()),
            io_timeout: DEFAULT_IO_TIMEOUT,
        }
    }

    #[must_use]
    pub fn with_io_timeout(mut self, io_timeout: Duration) -> Self {
        self.io_timeout = io_timeout;
        self
    }

    #[must_use]
    pub fn endpoint(&self) -> String {
        self.endpoint.lock().expect("endpoint mutex").clone()
    }

    pub fn set_endpoint(&self, endpoint: impl Into<String>) {
        *self.endpoint.lock().expect("endpoint mutex") = endpoint.into();
    }
}

impl ExchangeTransport for HttpExchangeTransport {
    fn exchange(&self, request_bytes: &[u8]) -> Result<Vec<u8>, ExchangeTransportError> {
        post_json(&self.endpoint(), request_bytes, self.io_timeout, &|| true)
    }
    fn exchange_authorized(
        &self,
        request_bytes: &[u8],
        can_start: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        post_json(&self.endpoint(), request_bytes, self.io_timeout, can_start)
    }
}

pub(crate) fn post_json(
    endpoint: &str,
    body: &[u8],
    io_timeout: Duration,
    can_start: &(dyn Fn() -> bool + Sync),
) -> Result<Vec<u8>, ExchangeTransportError> {
    validate_endpoint(endpoint)?;
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None)
        .timeout_global(Some(io_timeout))
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let response = winwincode_network::http::execute_http_once(
        &agent,
        |agent| {
            let request = agent
                .post(endpoint)
                .header("Content-Type", "application/json");
            request.send(body)
        },
        MAX_RESPONSE_BYTES as u64,
        io_timeout,
        can_start,
    )
    .map_err(ExchangeTransportError::from_network)?;
    if response.status().as_u16() != 200 {
        let delay = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| winwincode_network::retry_after(value, std::time::SystemTime::now()));
        return Err(ExchangeTransportError::from_network(
            winwincode_network::NetworkFailure::http(response.status().as_u16(), delay),
        ));
    }
    Ok(response.into_body())
}

fn invalid_endpoint() -> ExchangeTransportError {
    ExchangeTransportError::from_network(winwincode_network::NetworkFailure::new(
        winwincode_network::ErrorKind::RequestInvalid,
        winwincode_network::Acceptance::NotSent,
        winwincode_network::Phase::Connect,
    ))
}

fn validate_endpoint(endpoint: &str) -> Result<(), ExchangeTransportError> {
    let uri = http::Uri::from_str(endpoint).map_err(|_| invalid_endpoint())?;
    if uri.scheme_str() != Some("http") || uri.authority().is_none() {
        return Err(invalid_endpoint());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_endpoints_are_accepted() {
        assert!(validate_endpoint("http://127.0.0.1:8080/internal/v1/client/exchange").is_ok());
        assert!(validate_endpoint("https://server.example/internal/v1/client/exchange").is_err());
    }

    #[test]
    fn non_http_and_authority_less_endpoints_are_refused() {
        assert!(validate_endpoint("file:///tmp/exchange").is_err());
        assert!(validate_endpoint("127.0.0.1:8080/x").is_err());
        assert!(validate_endpoint("http:///x").is_err());
    }
}
