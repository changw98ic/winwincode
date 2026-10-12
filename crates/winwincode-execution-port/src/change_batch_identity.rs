// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use sha2::{Digest as _, Sha256};
use winwincode_domain::{ChangeBatchId, Sha256Digest};

use crate::generated::ChangeBatchIdentity;

const HASH_DOMAIN: &[u8] = b"winwincode.change-batch-id.v1\0";
const MAX_IDENTITY_PART_BYTES: usize = 200;

pub fn derive_change_batch_id(
    run_key: &str,
    turn_id: &str,
    call_id: Option<&str>,
    patch_digest: &Sha256Digest,
) -> Result<ChangeBatchId, ChangeBatchIdentityDerivationError> {
    validate_token(run_key).map_err(|()| ChangeBatchIdentityDerivationError::InvalidRunKey)?;
    validate_token(turn_id).map_err(|()| ChangeBatchIdentityDerivationError::InvalidTurnId)?;
    if call_id.is_some_and(|value| validate_token(value).is_err()) {
        return Err(ChangeBatchIdentityDerivationError::InvalidCallId);
    }
    if !is_sha256_digest(&patch_digest.0) {
        return Err(ChangeBatchIdentityDerivationError::InvalidPatchDigest);
    }
    Ok(hash_change_batch_parts(
        run_key,
        turn_id,
        call_id,
        &patch_digest.0,
    ))
}

pub fn validate_change_batch_identity_derivation(
    identity: &ChangeBatchIdentity,
) -> Result<(), ChangeBatchIdentityDerivationError> {
    let expected = derive_change_batch_id(
        &identity.run_key,
        &identity.turn_id,
        identity.call_id.as_deref(),
        &identity.patch_digest,
    )?;
    if identity.batch_id != expected {
        return Err(ChangeBatchIdentityDerivationError::BatchIdMismatch);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeBatchIdentityDerivationError {
    InvalidRunKey,
    InvalidTurnId,
    InvalidCallId,
    InvalidPatchDigest,
    BatchIdMismatch,
}

impl fmt::Display for ChangeBatchIdentityDerivationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRunKey => "ChangeBatch run key is invalid",
            Self::InvalidTurnId => "ChangeBatch turn ID is invalid",
            Self::InvalidCallId => "ChangeBatch call ID is invalid",
            Self::InvalidPatchDigest => "ChangeBatch patch digest is invalid",
            Self::BatchIdMismatch => "ChangeBatch ID does not match its content fields",
        })
    }
}

impl std::error::Error for ChangeBatchIdentityDerivationError {}

fn hash_change_batch_parts(
    run_key: &str,
    turn_id: &str,
    call_id: Option<&str>,
    patch_digest: &str,
) -> ChangeBatchId {
    let mut digest = Sha256::new();
    digest.update(HASH_DOMAIN);
    update_framed(&mut digest, run_key.as_bytes());
    update_framed(&mut digest, turn_id.as_bytes());
    match call_id {
        None => digest.update([0]),
        Some(call_id) => {
            digest.update([1]);
            update_framed(&mut digest, call_id.as_bytes());
        }
    }
    update_framed(&mut digest, patch_digest.as_bytes());
    ChangeBatchId(format!("sha256:{:x}", digest.finalize()))
}

fn update_framed(digest: &mut Sha256, value: &[u8]) {
    let length = u64::try_from(value.len()).expect("ChangeBatch identity part length fits u64");
    digest.update(length.to_be_bytes());
    digest.update(value);
}

fn validate_token(value: &str) -> Result<(), ()> {
    if value.is_empty() || value.len() > MAX_IDENTITY_PART_BYTES {
        return Err(());
    }
    let mut bytes = value.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !bytes.all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
    {
        return Err(());
    }
    Ok(())
}

fn is_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

#[cfg(test)]
mod tests {
    use super::hash_change_batch_parts;

    #[test]
    fn optional_call_tag_distinguishes_absent_from_present_empty() {
        let absent = hash_change_batch_parts("run", "turn", None, "sha256:patch");
        let present_empty = hash_change_batch_parts("run", "turn", Some(""), "sha256:patch");
        assert_ne!(absent, present_empty);
    }
}
