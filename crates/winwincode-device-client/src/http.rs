// SPDX-License-Identifier: Apache-2.0

//! Dependency-free std HTTP transport for the client exchange endpoint.
//!
//! One minimal HTTP/1.1 `POST` per exchange over a plain
//! [`TcpStream`](std::net::TcpStream): no async runtime, no TLS stack, no
//! external client dependency. The endpoint must therefore be an `http://`
//! URL; production deployments terminate TLS in front of the server (the
//! contract's HTTPS requirement is a deployment property, and the embedded
//! product composes this transport behind its own fronting proxy).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::Duration;

use crate::daemon::{ExchangeTransport, ExchangeTransportError};

/// Largest response body accepted from the endpoint (bounded batches of
/// bounded frames; far above the largest legal exchange response).
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
/// Default per-operation socket timeout.
const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimal std HTTP/1.1 `POST` implementation of [`ExchangeTransport`].
///
/// The daemon supplies the bearer credential per exchange, so the transport
/// itself holds no secret. The endpoint can be re-pointed at runtime
/// ([`HttpExchangeTransport::set_endpoint`]), e.g. when the server restarts
/// on a new port.
#[derive(Debug)]
pub struct HttpExchangeTransport {
    endpoint: Mutex<String>,
    io_timeout: Duration,
}

impl HttpExchangeTransport {
    /// Creates the transport for one exchange endpoint, e.g.
    /// `http://127.0.0.1:8080/internal/v1/client/exchange`.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: Mutex::new(endpoint.into()),
            io_timeout: DEFAULT_IO_TIMEOUT,
        }
    }

    /// Overrides the per-operation socket timeout.
    #[must_use]
    pub fn with_io_timeout(mut self, io_timeout: Duration) -> Self {
        self.io_timeout = io_timeout;
        self
    }

    /// The currently configured endpoint URL.
    ///
    /// # Panics
    ///
    /// Only when the endpoint mutex is poisoned (a previous holder panicked
    /// mid-update).
    #[must_use]
    pub fn endpoint(&self) -> String {
        self.endpoint.lock().expect("endpoint mutex").clone()
    }

    /// Re-points the transport at a new endpoint URL.
    ///
    /// # Panics
    ///
    /// Only when the endpoint mutex is poisoned (a previous holder panicked
    /// mid-update).
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
        let endpoint = self.endpoint();
        post_json(&endpoint, credential, request_bytes, self.io_timeout)
    }
}

/// Performs one blocking HTTP/1.1 `POST` of a JSON body and returns the
/// response body of a `200` response.
fn post_json(
    endpoint: &str,
    credential: Option<&str>,
    body: &[u8],
    io_timeout: Duration,
) -> Result<Vec<u8>, ExchangeTransportError> {
    let (authority, request_target) = split_endpoint(endpoint)?;
    let address = authority
        .to_socket_addrs()
        .map_err(|error| ExchangeTransportError::new(format!("endpoint {authority}: {error}")))?
        .next()
        .ok_or_else(|| {
            ExchangeTransportError::new(format!("endpoint {authority} resolves to no address"))
        })?;
    let mut stream = TcpStream::connect(address)
        .map_err(|error| ExchangeTransportError::new(format!("connect {address}: {error}")))?;
    stream
        .set_write_timeout(Some(io_timeout))
        .map_err(|error| ExchangeTransportError::new(format!("socket setup: {error}")))?;
    stream
        .set_read_timeout(Some(io_timeout))
        .map_err(|error| ExchangeTransportError::new(format!("socket setup: {error}")))?;

    let mut request = format!(
        "POST {request_target} HTTP/1.1\r\nHost: {authority}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(credential) = credential {
        request.push_str("Authorization: Bearer ");
        request.push_str(credential);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|error| ExchangeTransportError::new(format!("request write: {error}")))?;
    stream
        .write_all(body)
        .map_err(|error| ExchangeTransportError::new(format!("request write: {error}")))?;
    stream
        .flush()
        .map_err(|error| ExchangeTransportError::new(format!("request flush: {error}")))?;

    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut response)
        .map_err(|error| ExchangeTransportError::new(format!("response read: {error}")))?;
    if response.len() > MAX_RESPONSE_BYTES {
        return Err(ExchangeTransportError::new(
            "response exceeds the accepted body bound",
        ));
    }
    parse_response(&response)
}

/// Splits an `http://host:port/request-target` endpoint into the authority
/// and the request target. TLS endpoints are refused: this std transport
/// speaks plain HTTP only.
fn split_endpoint(endpoint: &str) -> Result<(String, String), ExchangeTransportError> {
    const SCHEME: &str = "http://";
    let Some(rest) = endpoint.strip_prefix(SCHEME) else {
        return Err(ExchangeTransportError::new(
            "the std http transport only accepts http:// exchange endpoints",
        ));
    };
    let (authority, target) = match rest.split_once('/') {
        Some((authority, path)) => (authority.to_owned(), format!("/{path}")),
        None => (rest.to_owned(), "/".to_owned()),
    };
    if authority.is_empty() || !authority.contains(':') {
        return Err(ExchangeTransportError::new(format!(
            "endpoint authority `{authority}` must be host:port"
        )));
    }
    Ok((authority, target))
}

/// Splits the status line and headers from the body, requires `200`, and
/// undoes `Content-Length` framing or `chunked` transfer encoding.
fn parse_response(response: &[u8]) -> Result<Vec<u8>, ExchangeTransportError> {
    let header_end = find_header_end(response).ok_or_else(|| {
        ExchangeTransportError::new("response carries no complete header section")
    })?;
    let headers = String::from_utf8_lossy(&response[..header_end]);
    let mut lines = headers.split("\r\n");
    let status = lines.next().unwrap_or_default().to_owned();
    if !status.starts_with("HTTP/1.1 200 ") && !status.starts_with("HTTP/1.0 200 ") {
        return Err(ExchangeTransportError::new(format!(
            "endpoint answered {status}"
        )));
    }
    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        } else if name == "content-length" {
            content_length = value.parse::<usize>().ok();
        }
    }
    let body = &response[header_end + 4..];
    if chunked {
        decode_chunked(body)
    } else if let Some(length) = content_length {
        if body.len() < length {
            return Err(ExchangeTransportError::new(
                "response body is shorter than its Content-Length",
            ));
        }
        Ok(body[..length].to_vec())
    } else {
        Ok(body.to_vec())
    }
}

fn find_header_end(response: &[u8]) -> Option<usize> {
    response.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Decodes a chunked response body (sizes in hex, `CRLF` framed, terminated
/// by the zero chunk).
fn decode_chunked(body: &[u8]) -> Result<Vec<u8>, ExchangeTransportError> {
    let mut decoded = Vec::new();
    let mut rest = body;
    loop {
        let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") else {
            return Err(ExchangeTransportError::new(
                "chunked response carries no chunk size line",
            ));
        };
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| {
            ExchangeTransportError::new("chunked response carries a bad chunk size")
        })?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(decoded);
        }
        if rest.len() < size + 2 {
            return Err(ExchangeTransportError::new(
                "chunked response ends inside a chunk",
            ));
        }
        decoded.extend_from_slice(&rest[..size]);
        rest = &rest[size..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
        if decoded.len() > MAX_RESPONSE_BYTES {
            return Err(ExchangeTransportError::new(
                "chunked response exceeds the accepted body bound",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport(endpoint: &str) -> Result<Vec<u8>, ExchangeTransportError> {
        split_endpoint(endpoint).map(|_| Vec::new())
    }

    #[test]
    fn http_endpoints_split_into_authority_and_target() {
        assert!(transport("http://127.0.0.1:8080/internal/v1/client/exchange").is_ok());
        assert!(transport("http://localhost:1/").is_ok());
    }

    #[test]
    fn non_http_and_authority_less_endpoints_are_refused() {
        assert!(transport("https://127.0.0.1:8080/x").is_err());
        assert!(transport("127.0.0.1:8080/x").is_err());
        assert!(transport("http:///x").is_err());
        assert!(transport("http://localhost/x").is_err());
    }

    #[test]
    fn content_length_responses_are_trimmed_to_length() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
             Content-Length: 4\r\nConnection: close\r\n\r\n{\"a\":1}";
        assert_eq!(parse_response(response).expect("body"), b"{\"a\"");
    }

    #[test]
    fn chunked_responses_are_decoded() {
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
             3\r\n{\"a\r\n4\r\n\":1}\r\n0\r\n\r\n";
        assert_eq!(
            parse_response(response).expect("body"),
            serde_json::json!({"a": 1}).to_string().as_bytes()
        );
    }

    #[test]
    fn non_200_status_is_a_transport_error_without_the_body() {
        let response = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n";
        let error = parse_response(response).expect_err("uniform rejection");
        assert!(error.message().contains("401"), "{error}");
    }
}
