// SPDX-License-Identifier: Apache-2.0

//! Renewable, fenced ownership for local Control Plane temporary roots.

use std::{
    fmt, fs,
    fs::OpenOptions,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const TEMPORARY_ROOT_LEASE_FILE: &str = ".winwincode-control-plane-lease.json";
const RECLAIM_CLAIM_FILE: &str = ".winwincode-control-plane-reclaim.json";
const RELEASE_CLAIM_FILE: &str = ".winwincode-control-plane-release.json";
const LEASE_SCHEMA: &str = "winwincode.control-plane-temporary-root-lease.v1";
const CLAIM_SCHEMA: &str = "winwincode.control-plane-temporary-root-reclaim.v1";
const RELEASE_SCHEMA: &str = "winwincode.control-plane-temporary-root-release.v1";
const INSTANCE_PREFIX: &str = "instance-";
const RECLAIM_PREFIX: &str = ".reclaim-";
const RELEASE_PREFIX: &str = ".release-";
const TOKEN_HEX_LEN: usize = 32;
const MAX_RECORD_BYTES: usize = 64 * 1024;
const DEFAULT_LEASE_MILLIS: u64 = 60_000;
const DEFAULT_STALE_GRACE_MILLIS: u64 = 60_000;
const DEFAULT_RENEW_INTERVAL_MILLIS: u64 = 20_000;

/// Supported local release target whose path lifecycle uses this lease.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TemporaryRootTarget {
    Aarch64AppleDarwin,
    X86_64AppleDarwin,
    Aarch64UnknownLinuxGnu,
    X86_64UnknownLinuxGnu,
}

impl TemporaryRootTarget {
    fn current() -> Result<Self, TemporaryRootLeaseError> {
        match (std::env::consts::ARCH, std::env::consts::OS) {
            ("aarch64", "macos") => Ok(Self::Aarch64AppleDarwin),
            ("x86_64", "macos") => Ok(Self::X86_64AppleDarwin),
            ("aarch64", "linux") => Ok(Self::Aarch64UnknownLinuxGnu),
            ("x86_64", "linux") => Ok(Self::X86_64UnknownLinuxGnu),
            _ => Err(TemporaryRootLeaseError::new(
                TemporaryRootLeaseErrorKind::UnsupportedTarget,
                "temporary root lease target is unsupported",
            )),
        }
    }
}

/// Bounded lease timing and platform configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TemporaryRootLeaseConfig {
    lease_millis: u64,
    stale_grace_millis: u64,
    renew_interval_millis: u64,
    target: TemporaryRootTarget,
}

impl TemporaryRootLeaseConfig {
    /// Creates a bounded configuration. Renewal must occur before expiry.
    ///
    /// # Errors
    ///
    /// Rejects zero durations, renewal at/after expiry, or arithmetic overflow.
    pub fn try_new(
        lease_millis: u64,
        stale_grace_millis: u64,
        renew_interval_millis: u64,
        target: TemporaryRootTarget,
    ) -> Result<Self, TemporaryRootLeaseError> {
        if lease_millis == 0
            || stale_grace_millis == 0
            || renew_interval_millis == 0
            || renew_interval_millis >= lease_millis
            || lease_millis.checked_add(stale_grace_millis).is_none()
        {
            return Err(TemporaryRootLeaseError::invalid());
        }
        Ok(Self {
            lease_millis,
            stale_grace_millis,
            renew_interval_millis,
            target,
        })
    }

    fn system() -> Result<Self, TemporaryRootLeaseError> {
        Self::try_new(
            DEFAULT_LEASE_MILLIS,
            DEFAULT_STALE_GRACE_MILLIS,
            DEFAULT_RENEW_INTERVAL_MILLIS,
            TemporaryRootTarget::current()?,
        )
    }
}

/// Stable temporary-root lease failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemporaryRootLeaseErrorKind {
    InvalidConfiguration,
    UnsupportedTarget,
    EntropyUnavailable,
    ClockUnavailable,
    PathOutsideParent,
    LeaseCorrupt,
    OwnershipLost,
    Io,
    RenewalTask,
}

/// Bounded temporary-root lease error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemporaryRootLeaseError {
    kind: TemporaryRootLeaseErrorKind,
    message: &'static str,
}

impl TemporaryRootLeaseError {
    const fn new(kind: TemporaryRootLeaseErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            TemporaryRootLeaseErrorKind::InvalidConfiguration,
            "temporary root lease configuration is invalid",
        )
    }

    const fn io() -> Self {
        Self::new(
            TemporaryRootLeaseErrorKind::Io,
            "temporary root lease filesystem operation failed",
        )
    }

    const fn corrupt() -> Self {
        Self::new(
            TemporaryRootLeaseErrorKind::LeaseCorrupt,
            "temporary root lease record is invalid",
        )
    }

    const fn ownership_lost() -> Self {
        Self::new(
            TemporaryRootLeaseErrorKind::OwnershipLost,
            "temporary root lease ownership was fenced",
        )
    }

    /// Creates the stable failure returned by an injected clock authority.
    #[must_use]
    pub const fn clock_unavailable() -> Self {
        Self::new(
            TemporaryRootLeaseErrorKind::ClockUnavailable,
            "temporary root lease clock is unavailable",
        )
    }

    /// Creates the stable failure returned by an injected entropy authority.
    #[must_use]
    pub const fn entropy_unavailable() -> Self {
        Self::new(
            TemporaryRootLeaseErrorKind::EntropyUnavailable,
            "temporary root lease entropy is unavailable",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> TemporaryRootLeaseErrorKind {
        self.kind
    }
}

impl fmt::Display for TemporaryRootLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for TemporaryRootLeaseError {}

/// Clock and entropy authority. Production uses `SystemTime` and OS entropy;
/// deterministic tests inject both without relying on PID or file timestamps.
pub trait TemporaryRootLeaseRuntime: Send + Sync {
    /// Returns the current Unix epoch millisecond.
    ///
    /// # Errors
    ///
    /// Returns `ClockUnavailable` when time cannot be read safely.
    fn now_millis(&self) -> Result<u64, TemporaryRootLeaseError>;

    /// Returns a fresh 128-bit random identity.
    ///
    /// # Errors
    ///
    /// Returns `EntropyUnavailable` when OS entropy cannot be read.
    fn random_128(&self) -> Result<[u8; 16], TemporaryRootLeaseError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemTemporaryRootLeaseRuntime;

impl TemporaryRootLeaseRuntime for SystemTemporaryRootLeaseRuntime {
    fn now_millis(&self) -> Result<u64, TemporaryRootLeaseError> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| TemporaryRootLeaseError::clock_unavailable())?
            .as_millis();
        u64::try_from(millis).map_err(|_| {
            TemporaryRootLeaseError::new(
                TemporaryRootLeaseErrorKind::ClockUnavailable,
                "temporary root lease clock is out of range",
            )
        })
    }

    fn random_128(&self) -> Result<[u8; 16], TemporaryRootLeaseError> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| TemporaryRootLeaseError::entropy_unavailable())?;
        Ok(bytes)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LeaseRecord {
    schema: String,
    instance_id: String,
    fencing_token: String,
    generation: u64,
    issued_at_millis: u64,
    expires_at_millis: u64,
    stale_after_millis: u64,
    target: TemporaryRootTarget,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReclaimClaim {
    schema: String,
    lease_sha256: String,
    reclaimer_id: String,
    claimed_at_millis: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReleaseClaim {
    schema: String,
    lease_sha256: String,
    releaser_id: String,
    released_at_millis: u64,
}

#[derive(Clone)]
pub struct TemporaryRootLeaseManager {
    parent: PathBuf,
    config: TemporaryRootLeaseConfig,
    runtime: Arc<dyn TemporaryRootLeaseRuntime>,
}

impl fmt::Debug for TemporaryRootLeaseManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TemporaryRootLeaseManager")
            .field("parent", &self.parent)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl TemporaryRootLeaseManager {
    /// Opens the manager under one exact canonical parent.
    ///
    /// # Errors
    ///
    /// Rejects a non-directory parent or filesystem failure.
    pub fn open(
        parent: impl AsRef<Path>,
        config: TemporaryRootLeaseConfig,
        runtime: Arc<dyn TemporaryRootLeaseRuntime>,
    ) -> Result<Self, TemporaryRootLeaseError> {
        fs::create_dir_all(parent.as_ref()).map_err(|_| TemporaryRootLeaseError::io())?;
        let parent =
            fs::canonicalize(parent.as_ref()).map_err(|_| TemporaryRootLeaseError::io())?;
        if !fs::metadata(&parent)
            .map_err(|_| TemporaryRootLeaseError::io())?
            .is_dir()
        {
            return Err(TemporaryRootLeaseError::new(
                TemporaryRootLeaseErrorKind::PathOutsideParent,
                "temporary root lease parent is not a directory",
            ));
        }
        Ok(Self {
            parent,
            config,
            runtime,
        })
    }

    /// Opens the production manager for the current release target.
    ///
    /// # Errors
    ///
    /// Returns a stable configuration or filesystem failure.
    pub fn system(parent: impl AsRef<Path>) -> Result<Self, TemporaryRootLeaseError> {
        Self::open(
            parent,
            TemporaryRootLeaseConfig::system()?,
            Arc::new(SystemTemporaryRootLeaseRuntime),
        )
    }

    /// Reclaims proven-stale roots, then creates one manually renewable lease.
    ///
    /// # Errors
    ///
    /// Returns a clock, entropy, lease validation, or filesystem failure.
    pub fn acquire(&self) -> Result<TemporaryRootLease, TemporaryRootLeaseError> {
        self.reclaim_expired()?;
        let now = self.runtime.now_millis()?;
        let expires_at_millis = checked_deadline(now, self.config.lease_millis)?;
        let stale_after_millis =
            checked_deadline(expires_at_millis, self.config.stale_grace_millis)?;
        for _attempt in 0..64 {
            let instance_id = random_token(self.runtime.as_ref())?;
            let fencing_token = random_token(self.runtime.as_ref())?;
            let path = self.parent.join(format!("{INSTANCE_PREFIX}{instance_id}"));
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => {
                    if fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).is_err() {
                        let _ = fs::remove_dir_all(&path);
                        return Err(TemporaryRootLeaseError::io());
                    }
                    let record = LeaseRecord {
                        schema: LEASE_SCHEMA.to_owned(),
                        instance_id,
                        fencing_token,
                        generation: 1,
                        issued_at_millis: now,
                        expires_at_millis,
                        stale_after_millis,
                        target: self.config.target,
                    };
                    let bytes = match encode_lease(&record) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            let _ = fs::remove_dir_all(&path);
                            return Err(error);
                        }
                    };
                    if let Err(error) = create_record(&path.join(TEMPORARY_ROOT_LEASE_FILE), &bytes)
                    {
                        let _ = fs::remove_dir_all(&path);
                        return Err(error);
                    }
                    return Ok(TemporaryRootLease {
                        manager: self.clone(),
                        path,
                        record,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_error) => return Err(TemporaryRootLeaseError::io()),
            }
        }
        Err(TemporaryRootLeaseError::new(
            TemporaryRootLeaseErrorKind::EntropyUnavailable,
            "temporary root instance identities repeatedly collided",
        ))
    }

    /// Creates a lease renewed by one lifecycle-owned thread.
    ///
    /// # Errors
    ///
    /// Returns an acquisition or renewal-thread startup failure.
    pub fn acquire_renewing(&self) -> Result<OwnedTemporaryRoot, TemporaryRootLeaseError> {
        OwnedTemporaryRoot::start(self.acquire()?)
    }

    /// Reclaims only roots whose exact canonical lease is beyond expiry plus grace.
    ///
    /// # Errors
    ///
    /// Returns a parent scan, clock, entropy, or atomic takeover failure.
    pub fn reclaim_expired(&self) -> Result<TemporaryRootReclaimReport, TemporaryRootLeaseError> {
        let mut report = TemporaryRootReclaimReport::default();
        for entry in fs::read_dir(&self.parent).map_err(|_| TemporaryRootLeaseError::io())? {
            let entry = entry.map_err(|_| TemporaryRootLeaseError::io())?;
            let file_type = entry
                .file_type()
                .map_err(|_| TemporaryRootLeaseError::io())?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                report.rejected += 1;
                continue;
            };
            if parse_instance_name(&name).is_some() {
                self.reclaim_instance(&entry.path(), &name, &mut report)?;
            } else if parse_quarantine_name(&name).is_some() {
                self.resume_quarantine(&entry.path(), &name, &mut report)?;
            } else if parse_release_name(&name).is_some() {
                self.resume_release(&entry.path(), &name, &mut report)?;
            }
        }
        Ok(report)
    }

    fn renew_handle(&self, handle: &mut LeaseHandle) -> Result<(), TemporaryRootLeaseError> {
        ensure_direct_child(&self.parent, &handle.path)?;
        let actual =
            read_lease(&handle.path).map_err(|_| TemporaryRootLeaseError::ownership_lost())?;
        if actual != handle.record || actual.target != self.config.target {
            return Err(TemporaryRootLeaseError::ownership_lost());
        }
        let now = self.runtime.now_millis()?;
        if now >= actual.expires_at_millis {
            return Err(TemporaryRootLeaseError::ownership_lost());
        }
        let issued_at_millis = now.max(actual.issued_at_millis);
        let candidate_base = if now < actual.issued_at_millis {
            actual.expires_at_millis
        } else {
            issued_at_millis
        };
        let expires_at_millis = actual
            .expires_at_millis
            .max(checked_deadline(candidate_base, self.config.lease_millis)?);
        let stale_after_millis =
            checked_deadline(expires_at_millis, self.config.stale_grace_millis)?;
        let next = LeaseRecord {
            generation: actual
                .generation
                .checked_add(1)
                .ok_or_else(TemporaryRootLeaseError::invalid)?,
            issued_at_millis,
            expires_at_millis,
            stale_after_millis,
            ..actual
        };
        atomic_replace_record(
            &handle.path,
            TEMPORARY_ROOT_LEASE_FILE,
            &encode_lease(&next)?,
            self.runtime.as_ref(),
        )?;
        if read_lease(&handle.path).map_err(|_| TemporaryRootLeaseError::ownership_lost())? != next
        {
            return Err(TemporaryRootLeaseError::ownership_lost());
        }
        handle.record = next;
        Ok(())
    }

    fn release_handle(&self, handle: &LeaseHandle) -> Result<(), TemporaryRootLeaseError> {
        ensure_direct_child(&self.parent, &handle.path)?;
        let lease_bytes = encode_lease(&handle.record)?;
        if read_bounded(&handle.path.join(TEMPORARY_ROOT_LEASE_FILE))
            .map_err(|_| TemporaryRootLeaseError::ownership_lost())?
            != lease_bytes
        {
            return Err(TemporaryRootLeaseError::ownership_lost());
        }
        let releaser_id = random_token(self.runtime.as_ref())?;
        let release_claim = ReleaseClaim {
            schema: RELEASE_SCHEMA.to_owned(),
            lease_sha256: sha256(&lease_bytes),
            releaser_id: releaser_id.clone(),
            released_at_millis: self.runtime.now_millis()?,
        };
        create_record(
            &handle.path.join(RELEASE_CLAIM_FILE),
            &serde_json::to_vec(&release_claim).map_err(|_| TemporaryRootLeaseError::corrupt())?,
        )?;
        let quarantine = self.parent.join(format!(
            "{RELEASE_PREFIX}{}-{}",
            handle.record.instance_id, releaser_id
        ));
        if fs::rename(&handle.path, &quarantine).is_err() {
            let _ = fs::remove_file(handle.path.join(RELEASE_CLAIM_FILE));
            return Err(TemporaryRootLeaseError::ownership_lost());
        }
        let result =
            Self::finish_release(&quarantine, &lease_bytes, &release_claim).map(|_removed| ());
        if result.is_err() && !handle.path.exists() {
            let _ = fs::rename(&quarantine, &handle.path);
        }
        result
    }

    fn finish_release(
        quarantine: &Path,
        expected_lease_bytes: &[u8],
        expected_claim: &ReleaseClaim,
    ) -> Result<bool, TemporaryRootLeaseError> {
        let Some(lease_bytes) =
            read_bounded_if_present(&quarantine.join(TEMPORARY_ROOT_LEASE_FILE))?
        else {
            return Ok(false);
        };
        let Some(claim_bytes) = read_bounded_if_present(&quarantine.join(RELEASE_CLAIM_FILE))?
        else {
            return Ok(false);
        };
        let claim = decode_canonical::<ReleaseClaim>(&claim_bytes)?;
        if lease_bytes != expected_lease_bytes || claim != *expected_claim {
            return Err(TemporaryRootLeaseError::ownership_lost());
        }
        remove_tree(quarantine)
    }

    fn reclaim_instance(
        &self,
        path: &Path,
        name: &str,
        report: &mut TemporaryRootReclaimReport,
    ) -> Result<(), TemporaryRootLeaseError> {
        let Some(instance_id) = parse_instance_name(name) else {
            report.rejected += 1;
            return Ok(());
        };
        let observed_bytes = match read_bounded(&path.join(TEMPORARY_ROOT_LEASE_FILE)) {
            Ok(bytes) => bytes,
            Err(_error) => {
                report.rejected += 1;
                return Ok(());
            }
        };
        let observed = match decode_lease(&observed_bytes) {
            Ok(record)
                if record.instance_id == instance_id && record.target == self.config.target =>
            {
                record
            }
            Ok(_) | Err(_) => {
                report.rejected += 1;
                return Ok(());
            }
        };
        if !self.is_stale(&observed)? {
            report.retained_active += 1;
            return Ok(());
        }
        let quarantine_name = format!(
            "{RECLAIM_PREFIX}{instance_id}-{}",
            random_token(self.runtime.as_ref())?
        );
        let quarantine = self.parent.join(&quarantine_name);
        match fs::rename(path, &quarantine) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                report.contested += 1;
                return Ok(());
            }
            Err(_error) => return Err(TemporaryRootLeaseError::io()),
        }
        let current_bytes = match read_bounded(&quarantine.join(TEMPORARY_ROOT_LEASE_FILE)) {
            Ok(bytes) => bytes,
            Err(_error) if !quarantine.exists() => {
                report.contested += 1;
                return Ok(());
            }
            Err(_error) => {
                restore_quarantine(&quarantine, path, report)?;
                report.rejected += 1;
                return Ok(());
            }
        };
        let current = match decode_lease(&current_bytes) {
            Ok(current) => current,
            Err(_error) => {
                restore_quarantine(&quarantine, path, report)?;
                report.rejected += 1;
                return Ok(());
            }
        };
        if current_bytes != observed_bytes
            || current.instance_id != instance_id
            || !self.is_stale(&current)?
        {
            restore_quarantine(&quarantine, path, report)?;
            return Ok(());
        }
        self.claim_and_remove(&quarantine, &quarantine_name, &current_bytes, report)
    }

    fn resume_quarantine(
        &self,
        path: &Path,
        name: &str,
        report: &mut TemporaryRootReclaimReport,
    ) -> Result<(), TemporaryRootLeaseError> {
        let Some((instance_id, _reclaimer_id)) = parse_quarantine_name(name) else {
            report.rejected += 1;
            return Ok(());
        };
        let bytes = match read_bounded(&path.join(TEMPORARY_ROOT_LEASE_FILE)) {
            Ok(bytes) => bytes,
            Err(_error) => {
                report.rejected += 1;
                return Ok(());
            }
        };
        let lease = match decode_lease(&bytes) {
            Ok(lease) if lease.instance_id == instance_id && lease.target == self.config.target => {
                lease
            }
            Ok(_) | Err(_) => {
                report.rejected += 1;
                return Ok(());
            }
        };
        if !self.is_stale(&lease)? {
            let original = self.parent.join(format!("{INSTANCE_PREFIX}{instance_id}"));
            restore_quarantine(path, &original, report)?;
            return Ok(());
        }
        self.claim_and_remove(path, name, &bytes, report)
    }

    fn resume_release(
        &self,
        path: &Path,
        name: &str,
        report: &mut TemporaryRootReclaimReport,
    ) -> Result<(), TemporaryRootLeaseError> {
        let Some((instance_id, releaser_id)) = parse_release_name(name) else {
            report.rejected += 1;
            return Ok(());
        };
        ensure_direct_child(&self.parent, path)?;
        let lease_bytes = match read_bounded(&path.join(TEMPORARY_ROOT_LEASE_FILE)) {
            Ok(bytes) => bytes,
            Err(_error) => {
                report.rejected += 1;
                return Ok(());
            }
        };
        if !matches!(
            decode_lease(&lease_bytes),
            Ok(lease)
                if lease.instance_id == instance_id && lease.target == self.config.target
        ) {
            report.rejected += 1;
            return Ok(());
        }
        let claim = match read_bounded(&path.join(RELEASE_CLAIM_FILE))
            .and_then(|bytes| decode_canonical::<ReleaseClaim>(&bytes))
        {
            Ok(claim)
                if claim.schema == RELEASE_SCHEMA
                    && claim.releaser_id == releaser_id
                    && claim.lease_sha256 == sha256(&lease_bytes) =>
            {
                claim
            }
            Ok(_) | Err(_) => {
                report.rejected += 1;
                return Ok(());
            }
        };
        if Self::finish_release(path, &lease_bytes, &claim)? {
            report.released += 1;
        } else {
            report.contested += 1;
        }
        Ok(())
    }

    fn claim_and_remove(
        &self,
        quarantine: &Path,
        quarantine_name: &str,
        lease_bytes: &[u8],
        report: &mut TemporaryRootReclaimReport,
    ) -> Result<(), TemporaryRootLeaseError> {
        let Some((_instance_id, reclaimer_id)) = parse_quarantine_name(quarantine_name) else {
            return Err(TemporaryRootLeaseError::new(
                TemporaryRootLeaseErrorKind::PathOutsideParent,
                "temporary root reclaim path is outside the lease namespace",
            ));
        };
        ensure_direct_child(&self.parent, quarantine)?;
        let claim_path = quarantine.join(RECLAIM_CLAIM_FILE);
        if !claim_path.exists() {
            let claim = ReclaimClaim {
                schema: CLAIM_SCHEMA.to_owned(),
                lease_sha256: sha256(lease_bytes),
                reclaimer_id: reclaimer_id.clone(),
                claimed_at_millis: self.runtime.now_millis()?,
            };
            let _created = create_record_if_absent(
                &claim_path,
                &serde_json::to_vec(&claim).map_err(|_| TemporaryRootLeaseError::corrupt())?,
            )?;
        }
        let claim: ReclaimClaim =
            match read_bounded(&claim_path).and_then(|bytes| decode_canonical(&bytes)) {
                Ok(claim) => claim,
                Err(_error) => {
                    report.rejected += 1;
                    return Ok(());
                }
            };
        if claim.schema != CLAIM_SCHEMA
            || claim.lease_sha256 != sha256(lease_bytes)
            || claim.reclaimer_id != reclaimer_id
        {
            report.rejected += 1;
            return Ok(());
        }
        if remove_tree(quarantine)? {
            report.reclaimed += 1;
        } else {
            report.contested += 1;
        }
        Ok(())
    }

    fn is_stale(&self, lease: &LeaseRecord) -> Result<bool, TemporaryRootLeaseError> {
        validate_lease(lease)?;
        Ok(self.runtime.now_millis()? >= lease.stale_after_millis)
    }
}

#[derive(Clone, Debug)]
struct LeaseHandle {
    path: PathBuf,
    record: LeaseRecord,
}

/// Manually renewable lease useful to a launcher that owns its scheduler.
pub struct TemporaryRootLease {
    manager: TemporaryRootLeaseManager,
    path: PathBuf,
    record: LeaseRecord,
}

impl TemporaryRootLease {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Renews this exact instance/fence generation atomically.
    ///
    /// # Errors
    ///
    /// Returns `OwnershipLost` for takeover, tampering, or another generation.
    pub fn renew(&mut self) -> Result<(), TemporaryRootLeaseError> {
        let mut handle = LeaseHandle {
            path: self.path.clone(),
            record: self.record.clone(),
        };
        self.manager.renew_handle(&mut handle)?;
        self.record = handle.record;
        Ok(())
    }

    /// Immediately removes this exact owned root through a fenced rename.
    ///
    /// # Errors
    ///
    /// Returns `OwnershipLost` rather than deleting a changed root.
    pub fn release(self) -> Result<(), TemporaryRootLeaseError> {
        let handle = LeaseHandle {
            path: self.path,
            record: self.record,
        };
        self.manager.release_handle(&handle)
    }

    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.record.instance_id
    }
}

#[derive(Default)]
struct RenewalState {
    stop: bool,
    failure: Option<TemporaryRootLeaseError>,
}

#[derive(Default)]
struct RenewalStatus {
    state: Mutex<RenewalState>,
    changed: Condvar,
}

/// Lifecycle-owned root with a renewable lease and joinable renewal task.
pub struct OwnedTemporaryRoot {
    path: PathBuf,
    handle: Arc<Mutex<Option<LeaseHandle>>>,
    manager: TemporaryRootLeaseManager,
    status: Arc<RenewalStatus>,
    task: Option<JoinHandle<()>>,
}

impl OwnedTemporaryRoot {
    pub(crate) fn create(parent: impl AsRef<Path>) -> Result<Self, TemporaryRootLeaseError> {
        TemporaryRootLeaseManager::system(parent)?.acquire_renewing()
    }

    fn start(lease: TemporaryRootLease) -> Result<Self, TemporaryRootLeaseError> {
        let path = lease.path.clone();
        let manager = lease.manager.clone();
        let handle = Arc::new(Mutex::new(Some(LeaseHandle {
            path: lease.path,
            record: lease.record,
        })));
        let status = Arc::new(RenewalStatus::default());
        let task_handle = Arc::clone(&handle);
        let task_status = Arc::clone(&status);
        let task_manager = manager.clone();
        let wait = Duration::from_millis(manager.config.renew_interval_millis);
        let task = match thread::Builder::new()
            .name("winwincode-temporary-root-renewal".to_owned())
            .spawn(move || renewal_loop(&task_manager, &task_handle, &task_status, wait))
        {
            Ok(task) => task,
            Err(_error) => {
                if let Ok(mut handle) = handle.lock()
                    && let Some(handle) = handle.take()
                {
                    let _ = manager.release_handle(&handle);
                }
                return Err(TemporaryRootLeaseError::new(
                    TemporaryRootLeaseErrorKind::RenewalTask,
                    "temporary root renewal task could not start",
                ));
            }
        };
        Ok(Self {
            path,
            handle,
            manager,
            status,
            task: Some(task),
        })
    }

    /// Returns the root only while the renewal task remains healthy.
    ///
    /// # Errors
    ///
    /// Returns the first renewal failure so callers stop using an unfenced
    /// root instead of silently continuing after the lease stopped advancing.
    pub fn path(&self) -> Result<&Path, TemporaryRootLeaseError> {
        let state = self
            .status
            .state
            .lock()
            .map_err(|_| TemporaryRootLeaseError::ownership_lost())?;
        if let Some(error) = &state.failure {
            return Err(error.clone());
        }
        Ok(&self.path)
    }

    /// Stops renewal and immediately removes the exact still-owned root.
    ///
    /// # Errors
    ///
    /// Returns a renewal-task, fencing, clock, entropy, or filesystem failure.
    pub fn release(mut self) -> Result<(), TemporaryRootLeaseError> {
        self.stop_task()?;
        let handle = self
            .handle
            .lock()
            .map_err(|_| TemporaryRootLeaseError::ownership_lost())?
            .take()
            .ok_or_else(TemporaryRootLeaseError::ownership_lost)?;
        self.manager.release_handle(&handle)
    }

    fn stop_task(&mut self) -> Result<(), TemporaryRootLeaseError> {
        if let Ok(mut state) = self.status.state.lock() {
            state.stop = true;
            self.status.changed.notify_all();
        }
        if let Some(task) = self.task.take() {
            task.join().map_err(|_| {
                TemporaryRootLeaseError::new(
                    TemporaryRootLeaseErrorKind::RenewalTask,
                    "temporary root renewal task did not close",
                )
            })?;
        }
        Ok(())
    }
}

impl Drop for OwnedTemporaryRoot {
    fn drop(&mut self) {
        let _ = self.stop_task();
    }
}

fn renewal_loop(
    manager: &TemporaryRootLeaseManager,
    handle: &Mutex<Option<LeaseHandle>>,
    status: &RenewalStatus,
    wait: Duration,
) {
    loop {
        let Ok(state) = status.state.lock() else {
            return;
        };
        if state.stop {
            return;
        }
        let Ok((state, _timeout)) = status.changed.wait_timeout(state, wait) else {
            return;
        };
        if state.stop {
            return;
        }
        drop(state);
        let Ok(mut handle) = handle.lock() else {
            record_renewal_failure(status, TemporaryRootLeaseError::ownership_lost());
            return;
        };
        let Some(handle) = handle.as_mut() else {
            return;
        };
        if let Err(error) = manager.renew_handle(handle) {
            record_renewal_failure(status, error);
            return;
        }
    }
}

fn record_renewal_failure(status: &RenewalStatus, error: TemporaryRootLeaseError) {
    if let Ok(mut state) = status.state.lock() {
        state.failure = Some(error);
        status.changed.notify_all();
    }
}

/// Result of one bounded stale-root scan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TemporaryRootReclaimReport {
    pub reclaimed: u64,
    pub released: u64,
    pub restored: u64,
    pub retained_active: u64,
    pub rejected: u64,
    pub contested: u64,
}

fn restore_quarantine(
    quarantine: &Path,
    original: &Path,
    report: &mut TemporaryRootReclaimReport,
) -> Result<(), TemporaryRootLeaseError> {
    if original.exists() {
        report.retained_active += 1;
        return Ok(());
    }
    match fs::rename(quarantine, original) {
        Ok(()) => report.restored += 1,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.contested += 1,
        Err(_error) => return Err(TemporaryRootLeaseError::io()),
    }
    Ok(())
}

fn checked_deadline(now: u64, duration: u64) -> Result<u64, TemporaryRootLeaseError> {
    now.checked_add(duration)
        .ok_or_else(TemporaryRootLeaseError::invalid)
}

fn random_token(
    runtime: &dyn TemporaryRootLeaseRuntime,
) -> Result<String, TemporaryRootLeaseError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = runtime.random_128()?;
    let mut token = String::with_capacity(TOKEN_HEX_LEN);
    for byte in bytes {
        token.push(char::from(HEX[usize::from(byte >> 4)]));
        token.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(token)
}

fn valid_token(value: &str) -> bool {
    value.len() == TOKEN_HEX_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn parse_instance_name(name: &str) -> Option<String> {
    let token = name.strip_prefix(INSTANCE_PREFIX)?;
    valid_token(token).then(|| token.to_owned())
}

fn parse_quarantine_name(name: &str) -> Option<(String, String)> {
    let value = name.strip_prefix(RECLAIM_PREFIX)?;
    let (instance, reclaimer) = value.split_once('-')?;
    (valid_token(instance) && valid_token(reclaimer))
        .then(|| (instance.to_owned(), reclaimer.to_owned()))
}

fn parse_release_name(name: &str) -> Option<(String, String)> {
    let value = name.strip_prefix(RELEASE_PREFIX)?;
    let (instance, releaser) = value.split_once('-')?;
    (valid_token(instance) && valid_token(releaser))
        .then(|| (instance.to_owned(), releaser.to_owned()))
}

fn ensure_direct_child(parent: &Path, path: &Path) -> Result<(), TemporaryRootLeaseError> {
    if path.parent() != Some(parent) {
        return Err(TemporaryRootLeaseError::new(
            TemporaryRootLeaseErrorKind::PathOutsideParent,
            "temporary root path is outside its canonical parent",
        ));
    }
    Ok(())
}

fn validate_lease(record: &LeaseRecord) -> Result<(), TemporaryRootLeaseError> {
    if record.schema != LEASE_SCHEMA
        || !valid_token(&record.instance_id)
        || !valid_token(&record.fencing_token)
        || record.generation == 0
        || record.expires_at_millis <= record.issued_at_millis
        || record.stale_after_millis <= record.expires_at_millis
    {
        return Err(TemporaryRootLeaseError::corrupt());
    }
    Ok(())
}

fn encode_lease(record: &LeaseRecord) -> Result<Vec<u8>, TemporaryRootLeaseError> {
    validate_lease(record)?;
    serde_json::to_vec(record).map_err(|_| TemporaryRootLeaseError::corrupt())
}

fn decode_lease(bytes: &[u8]) -> Result<LeaseRecord, TemporaryRootLeaseError> {
    let record: LeaseRecord = decode_canonical(bytes)?;
    validate_lease(&record)?;
    Ok(record)
}

fn decode_canonical<T>(bytes: &[u8]) -> Result<T, TemporaryRootLeaseError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    if bytes.is_empty() || bytes.len() > MAX_RECORD_BYTES {
        return Err(TemporaryRootLeaseError::corrupt());
    }
    let value: T = serde_json::from_slice(bytes).map_err(|_| TemporaryRootLeaseError::corrupt())?;
    if serde_json::to_vec(&value).map_err(|_| TemporaryRootLeaseError::corrupt())? != bytes {
        return Err(TemporaryRootLeaseError::corrupt());
    }
    Ok(value)
}

fn read_lease(root: &Path) -> Result<LeaseRecord, TemporaryRootLeaseError> {
    decode_lease(&read_bounded(&root.join(TEMPORARY_ROOT_LEASE_FILE))?)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, TemporaryRootLeaseError> {
    read_bounded_if_present(path)?.ok_or_else(TemporaryRootLeaseError::io)
}

fn read_bounded_if_present(path: &Path) -> Result<Option<Vec<u8>>, TemporaryRootLeaseError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_error) => return Err(TemporaryRootLeaseError::io()),
    };
    let max_record_bytes = u64::try_from(MAX_RECORD_BYTES).expect("record bound fits u64");
    if !metadata.file_type().is_file() || metadata.len() > max_record_bytes {
        return Err(TemporaryRootLeaseError::corrupt());
    }
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_error) => Err(TemporaryRootLeaseError::io()),
    }
}

fn create_record(path: &Path, bytes: &[u8]) -> Result<(), TemporaryRootLeaseError> {
    if !create_record_if_absent(path, bytes)? {
        return Err(TemporaryRootLeaseError::io());
    }
    Ok(())
}

fn create_record_if_absent(path: &Path, bytes: &[u8]) -> Result<bool, TemporaryRootLeaseError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path);
    let file = match file.as_mut() {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(_error) => return Err(TemporaryRootLeaseError::io()),
    };
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| TemporaryRootLeaseError::io())?;
    Ok(true)
}

fn remove_tree(path: &Path) -> Result<bool, TemporaryRootLeaseError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_error) => Err(TemporaryRootLeaseError::io()),
    }
}

fn atomic_replace_record(
    root: &Path,
    name: &str,
    bytes: &[u8],
    runtime: &dyn TemporaryRootLeaseRuntime,
) -> Result<(), TemporaryRootLeaseError> {
    let temporary = root.join(format!(".{name}.tmp-{}", random_token(runtime)?));
    create_record(&temporary, bytes)?;
    if let Err(_error) = fs::rename(&temporary, root.join(name)) {
        let _ = fs::remove_file(temporary);
        return Err(TemporaryRootLeaseError::ownership_lost());
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicU64, Ordering},
        sync::mpsc,
    };

    use super::*;

    struct ModeRuntime(AtomicU64);

    impl TemporaryRootLeaseRuntime for ModeRuntime {
        fn now_millis(&self) -> Result<u64, TemporaryRootLeaseError> {
            Ok(1_000)
        }

        fn random_128(&self) -> Result<[u8; 16], TemporaryRootLeaseError> {
            let value = self.0.fetch_add(1, Ordering::Relaxed);
            Ok(value.to_be_bytes().repeat(2).try_into().expect("16 bytes"))
        }
    }

    #[test]
    fn every_record_and_atomic_temporary_file_is_private() {
        let parent = std::env::temp_dir().join(format!(
            "winwincode-lease-mode-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("test clock")
                .as_nanos()
        ));
        let runtime = Arc::new(ModeRuntime(AtomicU64::new(1)));
        let runtime_port: Arc<dyn TemporaryRootLeaseRuntime> = runtime.clone();
        let manager = TemporaryRootLeaseManager::open(
            &parent,
            TemporaryRootLeaseConfig::try_new(100, 50, 5, TemporaryRootTarget::Aarch64AppleDarwin)
                .expect("test config"),
            runtime_port,
        )
        .expect("test manager");
        let lease = manager.acquire().expect("test lease");
        let claim = lease.path().join("claim-mode");
        let replaced = lease.path().join("atomic-mode");
        create_record(&claim, b"record").expect("claim helper");
        atomic_replace_record(lease.path(), "atomic-mode", b"record", runtime.as_ref())
            .expect("atomic helper");

        assert_eq!(
            fs::metadata(lease.path())
                .expect("root mode")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for record in [
            lease.path().join(TEMPORARY_ROOT_LEASE_FILE),
            claim,
            replaced,
        ] {
            assert_eq!(
                fs::metadata(record)
                    .expect("record mode")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        lease.release().expect("test release");
        fs::remove_dir_all(parent).expect("remove test parent");
    }

    #[test]
    fn concurrent_release_completion_treats_a_vanished_record_as_contested() {
        let parent = std::env::temp_dir().join(format!(
            "winwincode-lease-release-contention-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("test clock")
                .as_nanos()
        ));
        let runtime = Arc::new(ModeRuntime(AtomicU64::new(1)));
        let runtime_port: Arc<dyn TemporaryRootLeaseRuntime> = runtime.clone();
        let manager = TemporaryRootLeaseManager::open(
            &parent,
            TemporaryRootLeaseConfig::try_new(100, 50, 5, TemporaryRootTarget::Aarch64AppleDarwin)
                .expect("test config"),
            runtime_port,
        )
        .expect("test manager");
        let lease = manager.acquire().expect("test lease");
        let lease_bytes = encode_lease(&lease.record).expect("encode lease");
        let releaser_id = "f".repeat(TOKEN_HEX_LEN);
        let release_claim = ReleaseClaim {
            schema: RELEASE_SCHEMA.to_owned(),
            lease_sha256: sha256(&lease_bytes),
            releaser_id: releaser_id.clone(),
            released_at_millis: 1_000,
        };
        create_record(
            &lease.path.join(RELEASE_CLAIM_FILE),
            &serde_json::to_vec(&release_claim).expect("encode release claim"),
        )
        .expect("write release claim");
        let quarantine = parent.join(format!(
            "{RELEASE_PREFIX}{}-{releaser_id}",
            lease.record.instance_id
        ));
        fs::rename(&lease.path, &quarantine).expect("quarantine release");
        fs::remove_file(quarantine.join(TEMPORARY_ROOT_LEASE_FILE))
            .expect("simulate another verified finisher removing the record");

        assert!(
            !TemporaryRootLeaseManager::finish_release(&quarantine, &lease_bytes, &release_claim,)
                .expect("a competing exact release is not a filesystem failure")
        );

        fs::remove_dir_all(parent).expect("remove test parent");
    }

    #[test]
    fn renewal_task_observes_a_stop_requested_before_its_first_wait() {
        let parent = std::env::temp_dir().join(format!(
            "winwincode-lease-prewait-stop-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("test clock")
                .as_nanos()
        ));
        let runtime: Arc<dyn TemporaryRootLeaseRuntime> = Arc::new(ModeRuntime(AtomicU64::new(1)));
        let manager = TemporaryRootLeaseManager::open(
            &parent,
            TemporaryRootLeaseConfig::try_new(100, 50, 5, TemporaryRootTarget::Aarch64AppleDarwin)
                .expect("test config"),
            runtime,
        )
        .expect("test manager");
        let status = Arc::new(RenewalStatus::default());
        status.state.lock().expect("renewal state").stop = true;
        let task_status = Arc::clone(&status);
        let (finished, completion) = mpsc::channel();
        thread::spawn(move || {
            renewal_loop(
                &manager,
                &Mutex::new(None),
                &task_status,
                Duration::from_mins(1),
            );
            finished.send(()).expect("report renewal task completion");
        });

        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("a prior stop request must not wait for the renewal interval");
        fs::remove_dir_all(parent).expect("remove test parent");
    }
}
