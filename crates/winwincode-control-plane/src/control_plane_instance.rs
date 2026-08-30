// SPDX-License-Identifier: Apache-2.0

//! Control Plane instance lifecycle over the sole product-state authority.
//!
//! This facade generates process identities and applies bounded lease/drain
//! windows. Product commands are admitted and committed by the storage
//! ledger, so business consistency never depends on a process-local lock.

use std::fmt;
use std::path::{Path, PathBuf};

use winwincode_domain::Sha256Digest;
use winwincode_storage::{
    CommitReceipt, ControlPlaneCommandAdmission, ControlPlaneCommandClaim,
    ControlPlaneInstanceAuthority, ControlPlaneInstanceError, ControlPlaneInstanceErrorKind,
    ControlPlaneInstanceHealth, ControlPlaneInstanceIdentity, ReceiptIdentity, SqliteStorage,
    StateCommit,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Bounded renewable lease and graceful drain windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlPlaneInstanceRuntimeConfig {
    lease_duration_millis: u64,
    drain_timeout_millis: u64,
}

impl ControlPlaneInstanceRuntimeConfig {
    /// Builds one lifecycle configuration.
    ///
    /// # Errors
    ///
    /// Rejects zero or unsafe integer durations.
    pub fn try_new(
        lease_duration_millis: u64,
        drain_timeout_millis: u64,
    ) -> Result<Self, ControlPlaneInstanceRuntimeError> {
        if lease_duration_millis == 0
            || drain_timeout_millis == 0
            || lease_duration_millis > MAX_SAFE_INTEGER
            || drain_timeout_millis > MAX_SAFE_INTEGER
        {
            return Err(ControlPlaneInstanceRuntimeError::new(
                ControlPlaneInstanceRuntimeErrorKind::InvalidConfiguration,
                "Control Plane instance lease and drain windows must be positive safe integers",
            ));
        }
        Ok(Self {
            lease_duration_millis,
            drain_timeout_millis,
        })
    }

    #[must_use]
    pub const fn lease_duration_millis(self) -> u64 {
        self.lease_duration_millis
    }

    #[must_use]
    pub const fn drain_timeout_millis(self) -> u64 {
        self.drain_timeout_millis
    }
}

impl Default for ControlPlaneInstanceRuntimeConfig {
    fn default() -> Self {
        Self {
            lease_duration_millis: 30_000,
            drain_timeout_millis: 30_000,
        }
    }
}

/// Stable runtime failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlPlaneInstanceRuntimeErrorKind {
    InvalidConfiguration,
    EntropyUnavailable,
    Instance(ControlPlaneInstanceErrorKind),
}

/// Secret-free runtime error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPlaneInstanceRuntimeError {
    kind: ControlPlaneInstanceRuntimeErrorKind,
    message: String,
}

impl ControlPlaneInstanceRuntimeError {
    fn new(kind: ControlPlaneInstanceRuntimeErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ControlPlaneInstanceRuntimeErrorKind {
        self.kind
    }
}

impl fmt::Display for ControlPlaneInstanceRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ControlPlaneInstanceRuntimeError {}

impl From<ControlPlaneInstanceError> for ControlPlaneInstanceRuntimeError {
    fn from(source: ControlPlaneInstanceError) -> Self {
        Self::new(
            ControlPlaneInstanceRuntimeErrorKind::Instance(source.kind()),
            source.to_string(),
        )
    }
}

/// Running instance owner over one connection to the canonical product-state database.
pub struct ControlPlaneInstanceRuntime {
    storage: SqliteStorage,
    authority: ControlPlaneInstanceAuthority,
    config: ControlPlaneInstanceRuntimeConfig,
}

impl ControlPlaneInstanceRuntime {
    /// Starts one process with fresh random instance and boot identities.
    ///
    /// # Errors
    ///
    /// Returns a stable error when entropy, storage, schema, or registration fails.
    pub fn start(
        data_directory: impl AsRef<Path>,
        now: u64,
        config: ControlPlaneInstanceRuntimeConfig,
    ) -> Result<Self, ControlPlaneInstanceRuntimeError> {
        let identity = random_identity()?;
        Self::start_with_identity(data_directory, &identity, now, config)
    }

    /// Deterministic startup seam for deployment adapters and tests.
    ///
    /// # Errors
    ///
    /// Rejects invalid time arithmetic, storage failure, or conflicting ownership.
    pub fn start_with_identity(
        data_directory: impl AsRef<Path>,
        identity: &ControlPlaneInstanceIdentity,
        now: u64,
        config: ControlPlaneInstanceRuntimeConfig,
    ) -> Result<Self, ControlPlaneInstanceRuntimeError> {
        let lease_expires_at = deadline(now, config.lease_duration_millis())?;
        let mut storage = SqliteStorage::open(data_directory)
            .map_err(|source| instance_storage_error(&source))?;
        let authority =
            storage
                .control_plane_instance_ledger()?
                .register(identity, now, lease_expires_at)?;
        Ok(Self {
            storage,
            authority,
            config,
        })
    }

    #[must_use]
    pub const fn authority(&self) -> &ControlPlaneInstanceAuthority {
        &self.authority
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        self.storage.database_path()
    }

    /// Reads the durable readiness and drain projection.
    ///
    /// # Errors
    ///
    /// Rejects stale ownership, invalid time, corrupt state, or storage failure.
    pub fn preflight(
        &mut self,
        now: u64,
    ) -> Result<ControlPlaneInstanceHealth, ControlPlaneInstanceRuntimeError> {
        self.storage
            .control_plane_instance_ledger()?
            .preflight(&self.authority, now)
            .map_err(Into::into)
    }

    /// Renews this exact instance generation.
    ///
    /// # Errors
    ///
    /// Rejects late renewal, replaced ownership, invalid time arithmetic, or storage failure.
    pub fn renew(
        &mut self,
        now: u64,
    ) -> Result<ControlPlaneInstanceHealth, ControlPlaneInstanceRuntimeError> {
        let expires_at = deadline(now, self.config.lease_duration_millis())?;
        self.storage
            .control_plane_instance_ledger()?
            .renew(&self.authority, now, expires_at)
            .map_err(Into::into)
    }

    /// Stops accepting new commands and returns the first drain snapshot.
    ///
    /// # Errors
    ///
    /// Rejects stale/expired ownership, conflicting replay, invalid time, or storage failure.
    pub fn begin_drain(
        &mut self,
        now: u64,
    ) -> Result<ControlPlaneInstanceHealth, ControlPlaneInstanceRuntimeError> {
        let deadline = deadline(now, self.config.drain_timeout_millis())?;
        self.storage
            .control_plane_instance_ledger()?
            .request_drain(&self.authority, now, deadline)
            .map_err(Into::into)
    }

    /// Returns the current drain snapshot for bounded caller-side polling.
    ///
    /// # Errors
    ///
    /// Rejects stale authority, invalid time, corrupt state, or storage failure.
    pub fn await_drained(
        &mut self,
        now: u64,
    ) -> Result<ControlPlaneInstanceHealth, ControlPlaneInstanceRuntimeError> {
        self.preflight(now)
    }

    /// Cancels a still-live drain and resumes admissions.
    ///
    /// # Errors
    ///
    /// Rejects non-draining, expired, or replaced ownership and storage failure.
    pub fn resume(
        &mut self,
        now: u64,
    ) -> Result<ControlPlaneInstanceHealth, ControlPlaneInstanceRuntimeError> {
        self.storage
            .control_plane_instance_ledger()?
            .resume(&self.authority, now)
            .map_err(Into::into)
    }

    /// Permanently releases one fully drained generation.
    ///
    /// # Errors
    ///
    /// Rejects active commands, wrong lifecycle state, stale ownership, or storage failure.
    pub fn release(
        &mut self,
        now: u64,
    ) -> Result<ControlPlaneInstanceHealth, ControlPlaneInstanceRuntimeError> {
        self.storage
            .control_plane_instance_ledger()?
            .release(&self.authority, now)
            .map_err(Into::into)
    }

    /// Claims a not-yet-committed canonical command or returns its durable receipt projection.
    ///
    /// # Errors
    ///
    /// Rejects drain, stale authority, live foreign ownership, changed request reuse, or storage failure.
    pub fn admit_command(
        &mut self,
        now: u64,
        receipt_identity: &ReceiptIdentity,
        command_digest: &Sha256Digest,
    ) -> Result<ControlPlaneCommandAdmission, ControlPlaneInstanceRuntimeError> {
        self.storage
            .control_plane_instance_ledger()?
            .admit_command(&self.authority, now, receipt_identity, command_digest)
            .map_err(Into::into)
    }

    /// Commits one claimed command through the atomic instance-fenced path.
    ///
    /// # Errors
    ///
    /// Rejects stale claims/leases, changed commit facts, revisions, or storage failure.
    pub fn commit_claimed(
        &mut self,
        now: u64,
        claim: &ControlPlaneCommandClaim,
        commit: &StateCommit,
    ) -> Result<CommitReceipt, ControlPlaneInstanceRuntimeError> {
        self.storage
            .control_plane_instance_ledger()?
            .commit_claimed(claim, now, commit)
            .map_err(Into::into)
    }

    /// Returns the configured data directory used by deployment adapters.
    #[must_use]
    pub fn data_directory(&self) -> Option<PathBuf> {
        self.database_path().parent().map(Path::to_path_buf)
    }
}

fn random_identity() -> Result<ControlPlaneInstanceIdentity, ControlPlaneInstanceRuntimeError> {
    let mut entropy = [0_u8; 32];
    getrandom::fill(&mut entropy).map_err(|_| {
        ControlPlaneInstanceRuntimeError::new(
            ControlPlaneInstanceRuntimeErrorKind::EntropyUnavailable,
            "Control Plane instance identity entropy is unavailable",
        )
    })?;
    let instance_id = format!("cpi_{}", hex(&entropy[..16]));
    let boot_id = format!("cpb_{}", hex(&entropy[16..]));
    ControlPlaneInstanceIdentity::try_new(instance_id, boot_id).map_err(Into::into)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut value, "{byte:02x}").expect("writing hexadecimal to String cannot fail");
    }
    value
}

fn deadline(now: u64, duration: u64) -> Result<u64, ControlPlaneInstanceRuntimeError> {
    let deadline = now.checked_add(duration).ok_or_else(|| {
        ControlPlaneInstanceRuntimeError::new(
            ControlPlaneInstanceRuntimeErrorKind::InvalidConfiguration,
            "Control Plane lifecycle deadline overflowed",
        )
    })?;
    if deadline > MAX_SAFE_INTEGER {
        return Err(ControlPlaneInstanceRuntimeError::new(
            ControlPlaneInstanceRuntimeErrorKind::InvalidConfiguration,
            "Control Plane lifecycle deadline exceeds the safe integer range",
        ));
    }
    Ok(deadline)
}

fn instance_storage_error(
    source: &winwincode_storage::StorageError,
) -> ControlPlaneInstanceRuntimeError {
    ControlPlaneInstanceRuntimeError::new(
        ControlPlaneInstanceRuntimeErrorKind::Instance(ControlPlaneInstanceErrorKind::Storage),
        format!("Control Plane instance storage failed: {source}"),
    )
}
