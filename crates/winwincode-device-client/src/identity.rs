// SPDX-License-Identifier: Apache-2.0

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::store::{DeviceStore, DeviceStoreError, sql_error};

const IDENTITY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CANONICAL_ID_SUFFIX_LEN: usize = 26;
const MAX_ID_BYTES: usize = 200;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceIdentitySeed {
    pub display_name: String,
    pub platform: String,
    pub architecture: String,
    pub client_version: String,
}

#[allow(clippy::struct_field_names)]
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

    #[must_use]
    pub fn client_node_id(&self) -> &str {
        &self.client_node_id
    }

    #[must_use]
    pub fn public_client_id(&self) -> &str {
        &self.public_client_id
    }

    #[must_use]
    pub fn is_enrolled(&self) -> bool {
        !self.client_node_id.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IssuedEnrollment {
    pub client_node_id: String,
    pub public_client_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentityRecord {
    identity: DeviceIdentity,
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

struct StoredIdentity {
    identity: DeviceIdentity,
    created_at: String,
    revision: u64,
    fresh: bool,
}

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

    let revision = if stored.fresh { 1 } else { stored.revision + 1 };
    Ok(IdentityRecord {
        identity: stored.identity,
        current_instance_id,
        created_at: stored.created_at,
        revision,
    })
}

pub fn load_device_identity(
    store: &DeviceStore,
) -> Result<Option<IdentityRecord>, DeviceStoreError> {
    let connection = store.connection()?;
    let stored: Option<(String, String, String, String, i64)> = connection
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
        return Ok(None);
    };
    let revision = u64::try_from(revision)
        .map_err(|_| DeviceStoreError::adapter("stored identity revision is negative"))?;
    let current_instance_id: String = connection
        .query_row(
            "SELECT current_instance_id FROM device_identity WHERE device_id = ?1",
            [device_id.as_str()],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    Ok(Some(IdentityRecord {
        identity: DeviceIdentity {
            device_id,
            client_node_id,
            public_client_id,
        },
        current_instance_id,
        created_at,
        revision,
    }))
}

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
    transaction.commit().map_err(sql_error)
}

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

fn generate_identity() -> Result<DeviceIdentity, DeviceStoreError> {
    let mut device_id_bytes = [0_u8; 16];
    fill_random(&mut device_id_bytes)?;
    Ok(DeviceIdentity {
        device_id: format!("dvc_{}", hex_encode(&device_id_bytes)),
        client_node_id: String::new(),
        public_client_id: String::new(),
    })
}

pub(crate) fn generate_prefixed_id(prefix: &str) -> Result<String, DeviceStoreError> {
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
