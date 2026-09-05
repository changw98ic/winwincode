// SPDX-License-Identifier: Apache-2.0

//! Device identity and device credential: first-boot local generation and
//! durable persistence (plan sections 7.2, 11.2, and 17.1).
//!
//! Identifier lifetimes:
//!
//! - `device_id`: stable, purely local row identity, and the placeholder
//!   `clientNodeId` of the enrollment exchange (the server treats any
//!   non-canonical id as a fresh device).
//! - `client_node_id` / `publicClientId`: never generated locally. Both are
//!   server-issued with the accepted enrollment ([`adopt_enrollment`]) and
//!   backfilled into the persisted identity row; until then they are empty.
//! - `clientInstanceId`: a fresh canonical `cix_` + 26 character Crockford
//!   value on every process launch; the previously persisted value is always
//!   replaced on [`ensure_device_identity`].
//!
//! Credential model (plan 17.1): first boot creates a local random secret so
//! the durable row exists; the server-issued Device Credential replaces it at
//! enrollment. The issued material crosses the enrollment transport response
//! exactly once, is persisted here, and is presented as the exchange bearer
//! credential afterwards.

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::store::{DeviceStore, DeviceStoreError, sql_error};

/// Number of random bytes in the device credential secret.
const CREDENTIAL_SECRET_BYTES: usize = 32;
/// Crockford Base32 alphabet shared with the canonical identity encodings
/// (`I`, `L`, `O`, and `U` are excluded, matching the registry validation).
const IDENTITY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// A canonical prefixed identifier is prefix + 26 Crockford characters.
const CANONICAL_ID_SUFFIX_LEN: usize = 26;
const MAX_ID_BYTES: usize = 200;

/// Static description of this device recorded on first boot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceIdentitySeed {
    pub display_name: String,
    pub platform: String,
    pub architecture: String,
    pub client_version: String,
}

/// Stable device identifiers (plan sections 7.2 and 11.2).
///
/// `client_node_id` and `public_client_id` are empty until the enrollment is
/// adopted; both are server-issued and then stable across restarts.
#[allow(clippy::struct_field_names)] // every field is a distinct wire identifier
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceIdentity {
    device_id: String,
    client_node_id: String,
    public_client_id: String,
}

impl DeviceIdentity {
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The server-assigned `clientNodeId` (`cnd_` identity), or the empty
    /// string before the enrollment is adopted.
    #[must_use]
    pub fn client_node_id(&self) -> &str {
        &self.client_node_id
    }

    /// The server-assigned public `publicClientId`, or the empty string
    /// before the enrollment is adopted.
    #[must_use]
    pub fn public_client_id(&self) -> &str {
        &self.public_client_id
    }

    /// Whether the server-issued enrollment identity was adopted.
    #[must_use]
    pub fn is_enrolled(&self) -> bool {
        !self.client_node_id.is_empty()
    }
}

/// Locally persisted device credential. The secret never leaves the device
/// except as the exchange bearer credential; only [`DeviceCredential::digest`]
/// is persisted server-side.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceCredential {
    secret: Vec<u8>,
    digest: String,
    generation: u64,
}

impl DeviceCredential {
    /// Exposes the local credential secret bytes; they must never be logged.
    #[must_use]
    pub fn expose_secret(&self) -> &[u8] {
        &self.secret
    }

    /// The lowercase-hex presentation of the credential secret: the bearer
    /// material the exchange transport sends after enrollment. It must never
    /// be logged.
    #[must_use]
    pub fn material_hex(&self) -> String {
        hex_encode(&self.secret)
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

/// The server-issued enrollment identity carried by the exchange response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IssuedEnrollment {
    /// Server-assigned canonical `clientNodeId` (`cnd_` identity).
    pub client_node_id: String,
    /// Server-assigned public `publicClientId`.
    pub public_client_id: String,
    /// Raw Device Credential material as lowercase hex of the 32 secret
    /// bytes.
    pub credential_material: String,
    /// The persisted `sha256:` digest of the issued material.
    pub credential_digest: String,
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
    let current_instance_id = generate_prefixed_id("cix_")?;
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

/// Adopts the server-issued enrollment identity: backfills the persisted
/// identity row with the assigned `clientNodeId` and `publicClientId` and
/// replaces the local credential secret with the issued Device Credential.
///
/// Called exactly once, when the `client.enrollment_accepted` exchange
/// response arrives; a later call is refused so a replay can never rotate the
/// adopted identity.
///
/// # Errors
///
/// Returns
/// [`DeviceStoreErrorKind::InvalidInput`](crate::store::DeviceStoreErrorKind::InvalidInput)
/// for a non-canonical issued identity or credential material, and an
/// adapter-neutral error when the store is closed, the identity row is
/// missing, or the enrollment was already adopted.
pub fn adopt_enrollment(
    store: &mut DeviceStore,
    device_id: &str,
    issued: &IssuedEnrollment,
    adopted_at: &str,
) -> Result<(), DeviceStoreError> {
    validate_identifier(device_id, "device id")?;
    validate_identifier(adopted_at, "adopted at")?;
    if !is_canonical_prefixed_id(&issued.client_node_id, "cnd_") {
        return Err(DeviceStoreError::invalid(
            "issued clientNodeId is not a canonical cnd_ identity",
        ));
    }
    let public_client_id = &issued.public_client_id;
    if !(9..=12).contains(&public_client_id.len())
        || !public_client_id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(DeviceStoreError::invalid(
            "issued publicClientId must contain 9 to 12 digits",
        ));
    }
    let secret = decode_credential_material(&issued.credential_material)?;
    let digest = credential_digest(&secret);
    if digest != issued.credential_digest {
        return Err(DeviceStoreError::invalid(
            "issued credential digest does not match the issued material",
        ));
    }

    let connection = store.connection_mut()?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let adopted: Option<String> = transaction
        .query_row(
            "SELECT client_node_id FROM device_identity WHERE device_id = ?1",
            [device_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let Some(stored_node_id) = adopted else {
        return Err(DeviceStoreError::adapter(
            "the device identity row disappeared before the enrollment adoption",
        ));
    };
    if !stored_node_id.is_empty() {
        return Err(DeviceStoreError::conflict(
            "the enrollment identity was already adopted",
        ));
    }
    let changed = transaction
        .execute(
            "UPDATE device_identity \
             SET client_node_id = ?1, public_client_id = ?2, revision = revision + 1 \
             WHERE device_id = ?3",
            params![issued.client_node_id, issued.public_client_id, device_id],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(DeviceStoreError::adapter(
            "the device identity row update changed no rows",
        ));
    }
    let changed = transaction
        .execute(
            "UPDATE device_credential \
             SET credential_secret = ?1, credential_digest = ?2, \
             credential_generation = credential_generation + 1, rotated_at = ?3 \
             WHERE device_id = ?4",
            params![secret, digest, adopted_at, device_id],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(DeviceStoreError::adapter(
            "the device credential row update changed no rows",
        ));
    }
    transaction.commit().map_err(sql_error)
}

/// Loads the single identity row, or creates it inside the same transaction
/// on first boot.
fn load_or_create_identity(
    transaction: &rusqlite::Transaction<'_>,
    seed: &DeviceIdentitySeed,
    launched_at: &str,
    current_instance_id: &str,
) -> Result<StoredIdentity, DeviceStoreError> {
    let stored: Option<(String, String, String, String, i64)> = transaction
        .query_row(
            "SELECT device_id, client_node_id, public_client_id, created_at, revision \
             FROM device_identity",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    let Some((device_id, client_node_id, public_client_id, created_at, revision)) = stored else {
        return create_identity(transaction, seed, launched_at, current_instance_id);
    };
    let revision = u64::try_from(revision)
        .map_err(|_| DeviceStoreError::adapter("stored identity revision is negative"))?;
    Ok(StoredIdentity {
        identity: DeviceIdentity {
            device_id,
            client_node_id,
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
             (device_id, client_node_id, public_client_id, display_name, platform, \
              architecture, client_version, current_instance_id, created_at, revision) \
             VALUES (?1, '', '', ?2, ?3, ?4, ?5, ?6, ?7, 1)",
            params![
                identity.device_id(),
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
    // The local `device_id` is a random, purely local row identity and the
    // placeholder `clientNodeId` of the enrollment exchange; the server
    // treats every non-canonical id as a fresh device. `client_node_id` and
    // `public_client_id` stay empty until the server-issued enrollment
    // identity is adopted.
    let mut device_id_bytes = [0_u8; 16];
    fill_random(&mut device_id_bytes)?;
    Ok(DeviceIdentity {
        device_id: format!("dvc_{}", hex_encode(&device_id_bytes)),
        client_node_id: String::new(),
        public_client_id: String::new(),
    })
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

/// Generates one canonical `prefix` + 26 character Crockford identifier, the
/// same encoding the server assigns (`cnd_` node ids) and the registry
/// validates (`cix_` instance ids).
fn generate_prefixed_id(prefix: &str) -> Result<String, DeviceStoreError> {
    let mut random = [0_u8; 13];
    fill_random(&mut random)?;
    let mut identity = String::with_capacity(prefix.len() + CANONICAL_ID_SUFFIX_LEN);
    identity.push_str(prefix);
    for byte in random {
        identity.push(IDENTITY_ALPHABET[usize::from(byte >> 4)] as char);
        identity.push(IDENTITY_ALPHABET[usize::from(byte & 0x0F)] as char);
    }
    Ok(identity)
}

/// Whether `value` carries the canonical `prefix` + 26 character Crockford
/// shape the server-side registry validates.
fn is_canonical_prefixed_id(value: &str, prefix: &str) -> bool {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return false;
    };
    suffix.len() == CANONICAL_ID_SUFFIX_LEN
        && suffix.bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(
                    byte,
                    b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
                )
        })
}

/// Decodes the 64 lowercase-hex credential material into its 32 secret bytes.
fn decode_credential_material(material: &str) -> Result<Vec<u8>, DeviceStoreError> {
    let bytes = material.as_bytes();
    if bytes.len() != CREDENTIAL_SECRET_BYTES * 2 {
        return Err(DeviceStoreError::invalid(
            "issued credential material must be the hex of 32 bytes",
        ));
    }
    let mut secret = Vec::with_capacity(CREDENTIAL_SECRET_BYTES);
    for pair in bytes.chunks_exact(2) {
        let high = hex_nibble(pair[0])
            .ok_or_else(|| DeviceStoreError::invalid("issued credential material is not hex"))?;
        let low = hex_nibble(pair[1])
            .ok_or_else(|| DeviceStoreError::invalid("issued credential material is not hex"))?;
        secret.push(high << 4 | low);
    }
    Ok(secret)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
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
