// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;

use crate::daemon::{DeviceAuthRejection, ExchangeTransport, ExchangeTransportError};
use crate::remote_auth::DeviceRemoteAuth;

const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct HttpExchangeTransport {
    endpoint: Mutex<String>,
    io_timeout: Duration,
    auth: Option<DeviceRemoteAuth>,
    ca_roots: Vec<Vec<u8>>,
}

impl HttpExchangeTransport {
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: Mutex::new(endpoint.into()),
            io_timeout: DEFAULT_IO_TIMEOUT,
            auth: None,
            ca_roots: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_io_timeout(mut self, io_timeout: Duration) -> Self {
        self.io_timeout = io_timeout;
        self
    }

    #[must_use]
    pub fn with_device_auth(mut self, auth: DeviceRemoteAuth) -> Self {
        self.ca_roots = auth.ca_roots().to_vec();
        self.auth = Some(auth);
        self
    }

    #[must_use]
    pub fn with_ca_roots(mut self, roots: Vec<Vec<u8>>) -> Self {
        self.ca_roots = roots;
        self
    }

    #[must_use]
    pub fn endpoint(&self) -> String {
        self.endpoint.lock().expect("endpoint mutex").clone()
    }

    pub fn set_endpoint(&self, endpoint: impl Into<String>) {
        *self.endpoint.lock().expect("endpoint mutex") = endpoint.into();
    }

    fn post(
        &self,
        endpoint: &str,
        body: &[u8],
        can_start: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        if let Some(auth) = &self.auth {
            auth.post_json(endpoint, body, self.io_timeout, can_start)
        } else {
            post_json_with_options(
                endpoint,
                body,
                self.io_timeout,
                can_start,
                &self.ca_roots,
                &[],
            )
        }
    }
}

impl ExchangeTransport for HttpExchangeTransport {
    fn exchange(&self, request_bytes: &[u8]) -> Result<Vec<u8>, ExchangeTransportError> {
        self.post(&self.endpoint(), request_bytes, &|| true)
    }

    fn exchange_authorized(
        &self,
        request_bytes: &[u8],
        can_start: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        self.post(&self.endpoint(), request_bytes, can_start)
    }

    fn worker_exchange(&self, request_bytes: &[u8]) -> Result<Vec<u8>, ExchangeTransportError> {
        let endpoint = self.endpoint();
        let uri = http::Uri::from_str(&endpoint).map_err(|_| invalid_endpoint())?;
        let endpoint = format!(
            "{}://{}/internal/v1/client/worker-exchange",
            uri.scheme_str().ok_or_else(invalid_endpoint)?,
            uri.authority().ok_or_else(invalid_endpoint)?
        );
        self.post(&endpoint, request_bytes, &|| true)
    }
}

pub(crate) fn post_json(
    endpoint: &str,
    body: &[u8],
    io_timeout: Duration,
    can_start: &(dyn Fn() -> bool + Sync),
) -> Result<Vec<u8>, ExchangeTransportError> {
    post_json_with_options(endpoint, body, io_timeout, can_start, &[], &[])
}

pub(crate) fn post_json_with_options(
    endpoint: &str,
    body: &[u8],
    io_timeout: Duration,
    can_start: &(dyn Fn() -> bool + Sync),
    roots: &[Vec<u8>],
    headers: &[(String, String)],
) -> Result<Vec<u8>, ExchangeTransportError> {
    validate_endpoint(endpoint)?;
    let roots = if roots.is_empty() {
        ureq::tls::RootCerts::WebPki
    } else {
        roots
            .iter()
            .map(|root| ureq::tls::Certificate::from_der(root).to_owned())
            .collect::<Vec<_>>()
            .into()
    };
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None)
        .timeout_global(Some(io_timeout))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(roots)
                .use_sni(true)
                .disable_verification(false)
                .build(),
        )
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let response = winwincode_network::http::execute_http_once(
        &agent,
        |agent| {
            let mut request = agent
                .post(endpoint)
                .header("Content-Type", "application/json");
            for (name, value) in headers {
                request = request.header(name, value);
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
        let mut error = ExchangeTransportError::from_network(
            winwincode_network::NetworkFailure::http(response.status().as_u16(), delay),
        );
        if let Ok(response_error) = serde_json::from_slice::<
            winwincode_api::generated::DeviceAuthErrorResponse,
        >(response.body())
        {
            let rejection = match (response.status().as_u16(), response_error.code.as_str()) {
                (401, "DEVICE_PAIRING_REJECTED") => Some(DeviceAuthRejection::PairingRejected),
                (403, "DEVICE_REVOKED") => Some(DeviceAuthRejection::Revoked),
                _ => None,
            };
            if let Some(rejection) = rejection {
                error = error.with_device_auth_rejection(rejection);
            }
        }
        return Err(error);
    }
    Ok(response.into_body())
}

pub(crate) fn invalid_endpoint() -> ExchangeTransportError {
    ExchangeTransportError::from_network(winwincode_network::NetworkFailure::new(
        winwincode_network::ErrorKind::RequestInvalid,
        winwincode_network::Acceptance::NotSent,
        winwincode_network::Phase::Connect,
    ))
}

pub(crate) fn validate_endpoint(endpoint: &str) -> Result<(), ExchangeTransportError> {
    let uri = http::Uri::from_str(endpoint).map_err(|_| invalid_endpoint())?;
    let Some(authority) = uri.authority() else {
        return Err(invalid_endpoint());
    };
    if authority.as_str().contains('@') {
        return Err(invalid_endpoint());
    }
    match uri.scheme_str() {
        Some("https") => Ok(()),
        Some("http")
            if authority.host() == "localhost"
                || authority
                    .host()
                    .trim_matches(['[', ']'])
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback()) =>
        {
            Ok(())
        }
        _ => Err(invalid_endpoint()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_and_loopback_http_endpoints_are_accepted() {
        assert!(validate_endpoint("http://127.0.0.1:8080/internal/v1/client/exchange").is_ok());
        assert!(validate_endpoint("http://[::1]:8080/x").is_ok());
        assert!(validate_endpoint("https://server.example/internal/v1/client/exchange").is_ok());
        assert!(validate_endpoint("http://server.example/x").is_err());
        assert!(validate_endpoint("file:///tmp/exchange").is_err());
        assert!(validate_endpoint("127.0.0.1:8080/x").is_err());
        assert!(validate_endpoint("http:///x").is_err());
        assert!(validate_endpoint("https://user@server.example/x").is_err());
    }
}

#[cfg(test)]
mod tls_tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Arc;

    #[test]
    fn https_checks_certificate_and_hostname_with_explicit_device_ca() {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("TLS fixture certificate");
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key.into())
        .expect("TLS server");
        let listener = TcpListener::bind("127.0.0.1:0").expect("TLS fixture listener");
        let port = listener.local_addr().expect("fixture address").port();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (socket, _) = listener.accept().expect("TLS connection");
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("read timeout");
                let connection = rustls::ServerConnection::new(Arc::new(config.clone()))
                    .expect("TLS connection state");
                let mut stream = rustls::StreamOwned::new(connection, socket);
                let mut body = [0_u8; 4096];
                if stream.read(&mut body).is_ok() {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    );
                }
            }
        });
        let endpoint = format!("https://localhost:{port}/internal/v1/client/exchange");
        let certificate_path =
            std::env::temp_dir().join(format!("wwc-device-test-ca-{}.pem", std::process::id()));
        std::fs::write(&certificate_path, cert.pem()).expect("CA fixture file");
        let roots = crate::remote_auth::load_ca_roots(Some(&certificate_path))
            .expect("parse Device CA at its entry point");
        std::fs::remove_file(certificate_path).expect("remove CA fixture");
        assert_eq!(roots, vec![cert.der().to_vec()]);
        assert!(
            post_json_with_options(
                &endpoint,
                b"{}",
                Duration::from_secs(3),
                &|| true,
                &roots,
                &[]
            )
            .is_ok()
        );
        assert!(
            post_json_with_options(&endpoint, b"{}", Duration::from_secs(3), &|| true, &[], &[])
                .is_err(),
            "untrusted certificate is rejected"
        );
        let mismatch = format!("https://127.0.0.1:{port}/internal/v1/client/exchange");
        assert!(
            post_json_with_options(
                &mismatch,
                b"{}",
                Duration::from_secs(3),
                &|| true,
                &roots,
                &[]
            )
            .is_err(),
            "trusted CA does not disable hostname verification"
        );
        server.join().expect("TLS fixture thread");
    }
}
