// SPDX-License-Identifier: Apache-2.0

//! Bounded HTTP(S) transport for the client exchange endpoint.

use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;

use crate::daemon::{ExchangeTransport, ExchangeTransportError};

/// Largest response body accepted from the endpoint (bounded batches of
/// bounded frames; far above the largest legal exchange response).
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
/// Default total request timeout.
const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Blocking HTTP(S) implementation of [`ExchangeTransport`].
///
/// Redirects and environment proxies are disabled so the bearer credential
/// is sent only to the configured Server origin. TLS uses the WebPKI roots
/// provided by `ureq`'s Rustls backend.
#[derive(Debug)]
pub struct HttpExchangeTransport {
    endpoint: Mutex<String>,
    io_timeout: Duration,
    tls_root_der: Option<Vec<u8>>,
}

impl HttpExchangeTransport {
    /// Creates the transport for one absolute HTTP(S) exchange endpoint.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: Mutex::new(endpoint.into()),
            io_timeout: DEFAULT_IO_TIMEOUT,
            tls_root_der: None,
        }
    }

    /// Trusts one explicit DER-encoded Server root instead of the WebPKI set.
    #[must_use]
    pub fn with_tls_root_der(mut self, tls_root_der: Vec<u8>) -> Self {
        self.tls_root_der = Some(tls_root_der);
        self
    }

    /// Overrides the total request timeout.
    #[must_use]
    pub fn with_io_timeout(mut self, io_timeout: Duration) -> Self {
        self.io_timeout = io_timeout;
        self
    }

    /// The currently configured endpoint URL.
    ///
    /// # Panics
    ///
    /// Only when the endpoint mutex is poisoned.
    #[must_use]
    pub fn endpoint(&self) -> String {
        self.endpoint.lock().expect("endpoint mutex").clone()
    }

    /// Re-points the transport at a new endpoint URL.
    ///
    /// # Panics
    ///
    /// Only when the endpoint mutex is poisoned.
    pub fn set_endpoint(&self, endpoint: impl Into<String>) {
        *self.endpoint.lock().expect("endpoint mutex") = endpoint.into();
    }
}

impl ExchangeTransport for HttpExchangeTransport {
    fn exchange(
        &self,
        credential: Option<&str>,
        request_bytes: &[u8],
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        post_json(
            &self.endpoint(),
            credential,
            request_bytes,
            self.io_timeout,
            self.tls_root_der.as_deref(),
            &|| true,
        )
    }
    fn exchange_authorized(
        &self,
        credential: Option<&str>,
        request_bytes: &[u8],
        can_start: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        post_json(
            &self.endpoint(),
            credential,
            request_bytes,
            self.io_timeout,
            self.tls_root_der.as_deref(),
            can_start,
        )
    }
}

fn post_json(
    endpoint: &str,
    credential: Option<&str>,
    body: &[u8],
    io_timeout: Duration,
    tls_root_der: Option<&[u8]>,
    can_start: &(dyn Fn() -> bool + Sync),
) -> Result<Vec<u8>, ExchangeTransportError> {
    validate_endpoint(endpoint)?;
    let root_certs = tls_root_der.map_or(ureq::tls::RootCerts::WebPki, |value| {
        vec![ureq::tls::Certificate::from_der(value).to_owned()].into()
    });
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(root_certs)
        .use_sni(true)
        .disable_verification(false)
        .build();
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None)
        .timeout_global(Some(io_timeout))
        .tls_config(tls)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let response = winwincode_network::http::execute_http_once(
        &agent,
        |agent| {
            let mut request = agent
                .post(endpoint)
                .header("Content-Type", "application/json");
            if let Some(credential) = credential {
                request = request.header("Authorization", &format!("Bearer {credential}"));
            }
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
    if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.authority().is_none() {
        return Err(invalid_endpoint());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_and_https_endpoints_are_accepted() {
        assert!(validate_endpoint("http://127.0.0.1:8080/internal/v1/client/exchange").is_ok());
        assert!(validate_endpoint("https://server.example/internal/v1/client/exchange").is_ok());
    }

    #[test]
    fn non_http_and_authority_less_endpoints_are_refused() {
        assert!(validate_endpoint("file:///tmp/exchange").is_err());
        assert!(validate_endpoint("127.0.0.1:8080/x").is_err());
        assert!(validate_endpoint("http:///x").is_err());
    }
}
