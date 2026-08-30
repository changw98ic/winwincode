// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use http::Uri;

/// Public-listener TLS mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServerTls {
    Disabled,
    Pem {
        certificate_path: PathBuf,
        private_key_path: PathBuf,
    },
}

/// Complete standalone server configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerConfig {
    bind_address: SocketAddr,
    public_url: String,
    tls: ServerTls,
    allowed_origins: BTreeSet<String>,
    data_directory: PathBuf,
    shutdown_grace: Duration,
}

impl ServerConfig {
    /// Validate and construct one public-origin configuration.
    ///
    /// # Errors
    ///
    /// Rejects an invalid/mismatched URL, empty origin allowlist or storage
    /// path, a zero shutdown window, and incomplete TLS paths.
    pub fn new(
        bind_address: SocketAddr,
        public_url: impl Into<String>,
        tls: ServerTls,
        allowed_origins: impl IntoIterator<Item = String>,
        data_directory: PathBuf,
        shutdown_grace: Duration,
    ) -> Result<Self, ServerConfigError> {
        let public_url = normalized_origin(&public_url.into(), "publicUrl")?;
        let expected_scheme = match tls {
            ServerTls::Disabled => "http",
            ServerTls::Pem { .. } => "https",
        };
        if !public_url.starts_with(&format!("{expected_scheme}://")) {
            return Err(ServerConfigError::new(
                "publicUrl scheme must match the configured TLS mode",
            ));
        }
        validate_tls(&tls)?;
        let allowed_origins: BTreeSet<String> = allowed_origins
            .into_iter()
            .map(|origin| normalized_origin(&origin, "allowed origin"))
            .collect::<Result<_, _>>()?;
        if allowed_origins.is_empty() {
            return Err(ServerConfigError::new(
                "at least one browser origin must be allowed",
            ));
        }
        if data_directory.as_os_str().is_empty() {
            return Err(ServerConfigError::new("data directory must not be empty"));
        }
        if shutdown_grace.is_zero() {
            return Err(ServerConfigError::new(
                "shutdown grace period must be positive",
            ));
        }
        Ok(Self {
            bind_address,
            public_url,
            tls,
            allowed_origins,
            data_directory,
            shutdown_grace,
        })
    }

    #[must_use]
    pub const fn bind_address(&self) -> SocketAddr {
        self.bind_address
    }

    #[must_use]
    pub fn public_url(&self) -> &str {
        &self.public_url
    }

    #[must_use]
    pub const fn tls(&self) -> &ServerTls {
        &self.tls
    }

    #[must_use]
    pub const fn allowed_origins(&self) -> &BTreeSet<String> {
        &self.allowed_origins
    }

    #[must_use]
    pub fn data_directory(&self) -> &std::path::Path {
        &self.data_directory
    }

    #[must_use]
    pub const fn shutdown_grace(&self) -> Duration {
        self.shutdown_grace
    }
}

/// Configuration validation failure with no secret values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerConfigError {
    message: String,
}

impl ServerConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ServerConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ServerConfigError {}

fn validate_tls(tls: &ServerTls) -> Result<(), ServerConfigError> {
    if let ServerTls::Pem {
        certificate_path,
        private_key_path,
    } = tls
        && (certificate_path.as_os_str().is_empty() || private_key_path.as_os_str().is_empty())
    {
        return Err(ServerConfigError::new(
            "TLS certificate and private key paths must not be empty",
        ));
    }
    Ok(())
}

fn normalized_origin(value: &str, label: &str) -> Result<String, ServerConfigError> {
    let uri: Uri = value
        .parse()
        .map_err(|_| ServerConfigError::new(format!("{label} must be an absolute HTTP URL")))?;
    let scheme = uri
        .scheme_str()
        .filter(|scheme| matches!(*scheme, "http" | "https"))
        .ok_or_else(|| ServerConfigError::new(format!("{label} must use HTTP or HTTPS")))?;
    let authority = uri
        .authority()
        .ok_or_else(|| ServerConfigError::new(format!("{label} must include an authority")))?;
    if authority.as_str().contains('@') {
        return Err(ServerConfigError::new(format!(
            "{label} must not contain credentials"
        )));
    }
    if uri.query().is_some() || uri.path() != "/" && !uri.path().is_empty() {
        return Err(ServerConfigError::new(format!(
            "{label} must contain only an origin"
        )));
    }
    Ok(format!("{scheme}://{authority}"))
}
