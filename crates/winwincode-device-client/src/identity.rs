// SPDX-License-Identifier: Apache-2.0

//! Device identity and device credential: first-boot local generation and
//! durable persistence (plan sections 7.2 and 11.2).
//!
//! Three identifiers have distinct lifetimes:
//!
//! - `device_id`: stable, purely local row identity.
//! - `publicClientId`: stable across restarts, safe to publish, and used by
//!   users to find this device (plan section 11.2). PLACEHOLDER ALGORITHM:
//!   generated randomly on first boot and persisted; the stable encoding
//!   rules are owned by a later device-client lane and must be adopted
//!   without rotating existing values.
//! - `clientInstanceId`: a fresh value on every process launch; the
//!   previously persisted value is always replaced on
//!   [`ensure_device_identity`].
//!
//! The 32-byte credential secret is generated locally, persisted only in
//! this database, and never leaves the device; servers receive only its
//! SHA-256 digest (`deviceCredentialDigest`, plan section 7.2). The signing
//! and rotation protocol is owned by a later lane.

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::store::{DeviceStore, DeviceStoreError, sql_error};

/// Number of random bytes in the device credential secret.
const CREDENTIAL_SECRET_BYTES: usize = 32;
/// Placeholder `publicClientId` width (plan section 11.2 suggests 9-12
/// digits); the stable encoding is owned by a later lane.
const PUBLIC_CLIENT_ID_DIGITS: u32 = 10;
const MAX_ID_BYTES: usize = 200;

/// Static description of this device recorded on first boot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceIdentitySeed {
    pub display_name: String,
    pub platform: String,
    pub architecture: String,
    pub client_version: String,
}

/// Stable device identifiers (plan section 7.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceIdentity {
    device_id: String,
    public_client_id: String,
}

impl DeviceIdentity {
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    #[must_use]
    pub fn public_client_id(&self) -> &str {
        &self.public_client_id
    }
}

/// Locally persisted device credential. The secret never leaves the device;
/// only [`DeviceCredential::digest`] is shared with a server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceCredential {
    secret: Vec<u8>,
    digest: String,
    generation: u64,
}

impl DeviceCredential {
    /// Exposes the local credential secret for the future enrollment and
    /// rotation protocol; it must never be logged or uploaded.
    #[must_use]
    pub fn expose_secret(&self) -> &[u8] {
        &self.secret
    }

    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// The complete durable device identity after one startup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentityRecord {
    identity: DeviceIdentity,
    credential: DeviceCredential,
    current_instance_id: String,
    created_at: String,
    revision: u64,
}

impl IdentityRecord {
    #[must_use]
    pub const fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    #[must_use]
    pub const fn credential(&self) -> &DeviceCredential {
        &self.credential
    }

    /// The `clientInstanceId` for this process launch; a fresh value on
    /// every call to [`ensure_device_identity`].
    #[must_use]
    pub fn current_instance_id(&self) -> &str {
        &self.current_instance_id
    }

    #[must_use]
    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }
}

/// The identity row as stored, plus whether this launch created it.
struct StoredIdentity {
    identity: DeviceIdentity,
    created_at: String,
    revision: u64,
    fresh: bool,
}

/// Loads the durable device identity, creating it on first boot, and rotates
/// the `clientInstanceId` for this launch.
///
/// Must be called once per process start before any envelope is produced, so
/// every persisted or sent `clientInstanceId` names this launch.
///
/// # Errors
///
/// Returns
/// [`DeviceStoreErrorKind::InvalidInput`](crate::store::DeviceStoreErrorKind::InvalidInput)
/// for an empty seed or timestamp, and an adapter-neutral error when the
/// store is closed, the stored rows are inconsistent, or a write fails.
pub fn ensure_device_identity(
    store: &mut DeviceStore,
    seed: &DeviceIdentitySeed,
    launched_at: &str,
) -> Result<IdentityRecord, DeviceStoreError> {
    validate_seed(seed)?;
    validate_identifier(launched_at, "launched at")?;
    let current_instance_id = generate_instance_id()?;
    let connection = store.connection_mut()?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;

    let stored = load_or_create_identity(&transaction, seed, launched_at, &current_instance_id)?;
    let credential = load_or_create_credential(
        &transaction,
        &stored.identity.device_id,
        launched_at,
        stored.fresh,
    )?;
    if !stored.fresh {
        transaction
            .execute(
                "UPDATE device_identity SET current_instance_id = ?1, revision = revision + 1 \
                 WHERE device_id = ?2",
                params![current_instance_id, stored.identity.device_id],
            )
            .map_err(sql_error)?;
    }
    transaction.commit().map_err(sql_error)?;

    // The fresh insert already persisted revision 1 with this launch's
    // instance id; a restart rotates the row and reports the rotated
    // revision.
    let revision = if stored.fresh { 1 } else { stored.revision + 1 };
    Ok(IdentityRecord {
        identity: stored.identity,
        credential,
        current_instance_id,
        created_at: stored.created_at,
        revision,
    })
}

/// Loads the single identity row, or creates it inside the same transaction
/// on first boot.
fn load_or_create_identity(
    transaction: &rusqlite::Transaction<'_>,
    seed: &DeviceIdentitySeed,
    launched_at: &str,
    current_instance_id: &str,
) -> Result<StoredIdentity, DeviceStoreError> {
    let stored: Option<(String, String, String, i64)> = transaction
        .query_row(
            "SELECT device_id, public_client_id, created_at, revision FROM device_identity",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let Some((device_id, public_client_id, created_at, revision)) = stored else {
        return create_identity(transaction, seed, launched_at, current_instance_id);
    };
    let revision = u64::try_from(revision)
        .map_err(|_| DeviceStoreError::adapter("stored identity revision is negative"))?;
    Ok(StoredIdentity {
        identity: DeviceIdentity {
            device_id,
            public_client_id,
        },
        created_at,
        revision,
        fresh: false,
    })
}

fn create_identity(
    transaction: &rusqlite::Transaction<'_>,
    seed: &DeviceIdentitySeed,
    launched_at: &str,
    current_instance_id: &str,
) -> Result<StoredIdentity, DeviceStoreError> {
    let identity = generate_identity()?;
    transaction
        .execute(
            "INSERT INTO device_identity \
             (device_id, public_client_id, display_name, platform, architecture, \
              client_version, current_instance_id, created_at, revision) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1)",
            params![
                identity.device_id(),
                identity.public_client_id(),
                seed.display_name,
                seed.platform,
                seed.architecture,
                seed.client_version,
                current_instance_id,
                launched_at,
            ],
        )
        .map_err(|error| {
            DeviceStoreError::adapter(format!("device identity insert failed: {error}"))
        })?;
    Ok(StoredIdentity {
        identity,
        created_at: launched_at.to_owned(),
        revision: 1,
        fresh: true,
    })
}

/// Loads the durable credential for the identity, creating it inside the
/// same transaction on first boot.
///
/// The credential row must exist exactly with the identity row: it is
/// created in the same transaction, and the foreign key plus cascade delete
/// keep the pair consistent, so absence on a restart is a fail-closed
/// adapter fault.
fn load_or_create_credential(
    transaction: &rusqlite::Transaction<'_>,
    device_id: &str,
    launched_at: &str,
    fresh: bool,
) -> Result<DeviceCredential, DeviceStoreError> {
    let stored: Option<(Vec<u8>, String, i64)> = transaction
        .query_row(
            "SELECT credential_secret, credential_digest, credential_generation \
             FROM device_credential WHERE device_id = ?1",
            [device_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let Some((secret, digest, generation)) = stored else {
        if !fresh {
            return Err(DeviceStoreError::adapter(
                "device identity exists without a durable credential",
            ));
        }
        return create_credential(transaction, device_id, launched_at);
    };
    let generation = u64::try_from(generation)
        .map_err(|_| DeviceStoreError::adapter("stored credential generation is negative"))?;
    validate_credential_rows(device_id, &secret, &digest, generation)?;
    Ok(DeviceCredential {
        secret,
        digest,
        generation,
    })
}

fn create_credential(
    transaction: &rusqlite::Transaction<'_>,
    device_id: &str,
    launched_at: &str,
) -> Result<DeviceCredential, DeviceStoreError> {
    let credential = generate_credential()?;
    transaction
        .execute(
            "INSERT INTO device_credential \
             (device_id, credential_secret, credential_digest, credential_generation, \
              rotated_at) \
             VALUES (?1, ?2, ?3, 1, ?4)",
            params![device_id, credential.secret, credential.digest, launched_at,],
        )
        .map_err(|error| {
            DeviceStoreError::adapter(format!("device credential insert failed: {error}"))
        })?;
    Ok(credential)
}

fn validate_seed(seed: &DeviceIdentitySeed) -> Result<(), DeviceStoreError> {
    let DeviceIdentitySeed {
        display_name,
        platform,
        architecture,
        client_version,
    } = seed;
    for (value, label) in [
        (display_name.as_str(), "display name"),
        (platform.as_str(), "platform"),
        (architecture.as_str(), "architecture"),
        (client_version.as_str(), "client version"),
    ] {
        validate_identifier(value, label)?;
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<(), DeviceStoreError> {
    if value.is_empty() {
        return Err(DeviceStoreError::invalid(format!(
            "{label} must not be empty"
        )));
    }
    if value.len() > MAX_ID_BYTES {
        return Err(DeviceStoreError::invalid(format!(
            "{label} must contain at most {MAX_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_credential_rows(
    device_id: &str,
    secret: &[u8],
    digest: &str,
    generation: u64,
) -> Result<(), DeviceStoreError> {
    if secret.len() != CREDENTIAL_SECRET_BYTES {
        return Err(DeviceStoreError::adapter(format!(
            "device credential for {device_id} is not {CREDENTIAL_SECRET_BYTES} bytes"
        )));
    }
    if digest != credential_digest(secret) {
        return Err(DeviceStoreError::adapter(format!(
            "device credential digest for {device_id} does not match its secret"
        )));
    }
    if generation == 0 {
        return Err(DeviceStoreError::adapter(format!(
            "device credential generation for {device_id} is not positive"
        )));
    }
    Ok(())
}

fn generate_identity() -> Result<DeviceIdentity, DeviceStoreError> {
    // PLACEHOLDER ALGORITHMS: `device_id` and `publicClientId` are random on
    // first boot and then persisted unchanged. The stable encoding rules
    // (plan section 11.2) are owned by a later device-client lane.
    let mut device_id_bytes = [0_u8; 16];
    fill_random(&mut device_id_bytes)?;
    let device_id = format!("dvc_{}", hex_encode(&device_id_bytes));
    let public_client_id = generate_public_client_id()?;
    Ok(DeviceIdentity {
        device_id,
        public_client_id,
    })
}

fn generate_public_client_id() -> Result<String, DeviceStoreError> {
    let mut bytes = [0_u8; 8];
    fill_random(&mut bytes)?;
    let value = u64::from_be_bytes(bytes) % 10_u64.pow(PUBLIC_CLIENT_ID_DIGITS);
    Ok(format!("{value:0>10}"))
}

fn generate_credential() -> Result<DeviceCredential, DeviceStoreError> {
    let mut secret = vec![0_u8; CREDENTIAL_SECRET_BYTES];
    fill_random(&mut secret)?;
    let digest = credential_digest(&secret);
    Ok(DeviceCredential {
        secret,
        digest,
        generation: 1,
    })
}

fn credential_digest(secret: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(secret))
}

/// Generates the `clientInstanceId` for one process launch (plan section
/// 9.5). PLACEHOLDER ALGORITHM: 16 random bytes as lowercase hex; the
/// stable encoding is owned by a later device-client lane.
fn generate_instance_id() -> Result<String, DeviceStoreError> {
    let mut bytes = [0_u8; 16];
    fill_random(&mut bytes)?;
    Ok(format!("inst_{}", hex_encode(&bytes)))
}

fn fill_random(buffer: &mut [u8]) -> Result<(), DeviceStoreError> {
    getrandom::fill(buffer).map_err(|error| {
        DeviceStoreError::adapter(format!("device client entropy failure: {error}"))
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from_digit(u32::from(byte >> 4), 16).expect("high nibble"));
        encoded.push(char::from_digit(u32::from(byte & 0x0F), 16).expect("low nibble"));
    }
    encoded
}
