//! Cross-process serialization for one MCP OAuth credential's refresh transaction.
//!
//! The guard is intentionally acquired before the authoritative credential reread and retained
//! through provider refresh and persistence. This prevents two processes from replaying the same
//! rotating refresh token or observing a partially persisted transaction.

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use codex_utils_home_dir::find_codex_home;
use sha2::Digest;
use sha2::Sha256;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use std::time::Duration;
use tokio::time::sleep;
use tokio::time::timeout;

const REFRESH_LOCK_DIR: &str = "mcp-oauth-locks";
const REFRESH_LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 60);
const REFRESH_LOCK_RETRY_SLEEP: Duration = Duration::from_millis(/*millis*/ 50);
// Keep this internal target stable so diagnostics and cross-process tests can distinguish actual
// WouldBlock contention from a contender that merely started late and observed persisted tokens.
const LOCK_CONTENTION_EVENT_TARGET: &str = "codex_rmcp_client::oauth::refresh_lock::contention";

pub(crate) struct RefreshCredentialLock {
    file: File,
}

impl RefreshCredentialLock {
    pub(crate) async fn acquire_for_server(server_name: &str, url: &str) -> Result<Self> {
        let store_key = super::compute_store_key(server_name, url)?;
        let codex_home = find_codex_home()?;
        Self::acquire_in(&codex_home, &store_key, REFRESH_LOCK_ACQUIRE_TIMEOUT)
            .await
            .with_context(|| format!("failed to acquire OAuth credential lock for {server_name}"))
    }

    /// A durable digest fences an unknown rotating-token outcome across restarts.
    pub(crate) fn check_refresh_outcome(&mut self, fingerprint: &str) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        self.file.seek(SeekFrom::Start(0))?;
        let mut retained = String::new();
        (&self.file).take(65).read_to_string(&mut retained)?;
        if retained.is_empty() {
            return Ok(());
        }
        if retained.len() != 64 || retained == fingerprint {
            return Err(rmcp::transport::auth::AuthError::AuthorizationRequired)
                .context("MCP OAuth refresh acceptance is unknown; reauthorization required");
        }
        // A different authoritative credential proves a completed refresh or reauthorization.
        self.clear_refresh_outcome()
    }
    pub(crate) fn begin_refresh(&mut self, fingerprint: &str) -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        self.file.seek(SeekFrom::Start(0))?;
        self.file.set_len(0)?;
        self.file.write_all(fingerprint.as_bytes())?;
        self.file.sync_all()?;
        Ok(())
    }
    pub(crate) fn clear_refresh_outcome(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.sync_all()?;
        Ok(())
    }

    async fn acquire_in(
        codex_home: &Path,
        store_key: &str,
        acquire_timeout: Duration,
    ) -> Result<Self> {
        // Scope coordination to CODEX_HOME alongside File and Secrets state. Direct keyring
        // coordination across homes needs a separate cross-platform rendezvous.
        // TODO(stevenlee): define that rendezvous before expanding this lock's scope.
        let mut hasher = Sha256::new();
        hasher.update(store_key.as_bytes());
        let path = codex_home
            .join(REFRESH_LOCK_DIR)
            .join(format!("{:x}.lock", hasher.finalize()));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open OAuth refresh lock {}", path.display()))?;

        // Bound every contender, but keep the acquired lock for the full provider request and
        // persistence transaction. Releasing it while awaiting the provider would allow concurrent
        // use of a rotating refresh token.
        let mut reported_contention = false;
        timeout(acquire_timeout, async {
            loop {
                match file.try_lock() {
                    Ok(()) => return Ok(()),
                    Err(std::fs::TryLockError::WouldBlock) => {
                        if !reported_contention {
                            tracing::debug!(
                                target: LOCK_CONTENTION_EVENT_TARGET,
                                lock_path = %path.display(),
                                "waiting for another process to finish refreshing MCP OAuth credentials"
                            );
                            reported_contention = true;
                        }
                        sleep(REFRESH_LOCK_RETRY_SLEEP).await;
                    }
                    Err(error) => return Err(std::io::Error::from(error)),
                }
            }
        })
        .await
        .map_err(|_| {
            anyhow!(
                "timed out after {acquire_timeout:?} waiting for OAuth refresh lock {}",
                path.display()
            )
        })?
        .with_context(|| format!("failed to lock OAuth refresh lock {}", path.display()))?;

        Ok(Self { file })
    }
}

#[cfg(test)]
#[path = "refresh_lock_tests.rs"]
mod tests;
