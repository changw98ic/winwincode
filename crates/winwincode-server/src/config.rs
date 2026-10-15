// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use http::Uri;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerConfig {
    bind_address: SocketAddr,
    public_url: String,
    allowed_origins: BTreeSet<String>,
    preview_public_url: Option<String>,
    data_directory: PathBuf,
    shutdown_grace: Duration,
    tls_pem: Option<(PathBuf, PathBuf)>,
    tls_proxy: bool,
}

impl ServerConfig {
    pub fn new(
        bind_address: SocketAddr,
        public_url: impl Into<String>,
        allowed_origins: impl IntoIterator<Item = String>,
        data_directory: PathBuf,
        shutdown_grace: Duration,
    ) -> Result<Self, ServerConfigError> {
        let public_url = normalized_origin(&public_url.into(), "publicUrl")?;
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
            allowed_origins,
            preview_public_url: None,
            data_directory,
            shutdown_grace,
            tls_pem: None,
            tls_proxy: false,
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
    pub const fn allowed_origins(&self) -> &BTreeSet<String> {
        &self.allowed_origins
    }

    pub fn with_preview_public_url(
        mut self,
        preview_public_url: impl Into<String>,
    ) -> Result<Self, ServerConfigError> {
        let preview_public_url = normalized_origin(&preview_public_url.into(), "previewPublicUrl")?;
        if preview_public_url == self.public_url {
            return Err(ServerConfigError::new(
                "previewPublicUrl must use an origin separate from publicUrl",
            ));
        }
        self.preview_public_url = Some(preview_public_url);
        Ok(self)
    }

    #[must_use]
    pub fn preview_public_url(&self) -> Option<&str> {
        self.preview_public_url.as_deref()
    }

    #[must_use]
    pub fn data_directory(&self) -> &std::path::Path {
        &self.data_directory
    }

    #[must_use]
    pub const fn shutdown_grace(&self) -> Duration {
        self.shutdown_grace
    }

    #[must_use]
    pub fn remote(&self) -> bool {
        !self.bind_address.ip().is_loopback() || self.public_url.starts_with("https://")
    }

    pub fn with_tls_pem(
        mut self,
        certificate: PathBuf,
        private_key: PathBuf,
    ) -> Result<Self, ServerConfigError> {
        if !self.public_url.starts_with("https://") || self.tls_proxy {
            return Err(ServerConfigError::new(
                "direct TLS requires an HTTPS public URL and no TLS proxy",
            ));
        }
        self.tls_pem = Some((certificate, private_key));
        Ok(self)
    }

    pub fn with_loopback_tls_proxy(mut self) -> Result<Self, ServerConfigError> {
        if !self.bind_address.ip().is_loopback()
            || !self.public_url.starts_with("https://")
            || self.tls_pem.is_some()
        {
            return Err(ServerConfigError::new(
                "TLS proxy requires a protected loopback backend and an HTTPS public URL",
            ));
        }
        self.tls_proxy = true;
        Ok(self)
    }

    pub(crate) fn tls_pem(&self) -> Option<&(PathBuf, PathBuf)> {
        self.tls_pem.as_ref()
    }

    pub(crate) fn validate_transport(&self) -> Result<(), ServerConfigError> {
        if self.remote()
            && (!self.public_url.starts_with("https://")
                || self.tls_pem.is_none() && !self.tls_proxy)
        {
            return Err(ServerConfigError::new(
                "remote Server requires direct TLS or an explicitly protected TLS proxy",
            ));
        }
        Ok(())
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ServerConfig {
        ServerConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "http://control.example",
            ["http://app.example".to_owned()],
            PathBuf::from("data"),
            Duration::from_secs(1),
        )
        .unwrap()
    }

    #[test]
    fn remote_requires_tls_and_proxy_backend_remains_remote() {
        let remote = ServerConfig::new(
            "0.0.0.0:8443".parse().unwrap(),
            "http://control.example",
            ["https://client.example".to_owned()],
            PathBuf::from("data"),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(remote.validate_transport().is_err());
        let remote = ServerConfig::new(
            "127.0.0.1:8443".parse().unwrap(),
            "https://control.example",
            ["https://client.example".to_owned()],
            PathBuf::from("data"),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(remote.remote());
        assert!(remote.validate_transport().is_err());
        assert!(
            remote
                .with_loopback_tls_proxy()
                .unwrap()
                .validate_transport()
                .is_ok()
        );
        assert!(config().with_loopback_tls_proxy().is_err());
    }

    #[test]
    fn preview_origin_is_explicit_and_separate() {
        assert!(
            config()
                .with_preview_public_url("http://control.example")
                .is_err()
        );
        assert_eq!(
            config()
                .with_preview_public_url("http://preview.example")
                .unwrap()
                .preview_public_url(),
            Some("http://preview.example")
        );
    }
}
