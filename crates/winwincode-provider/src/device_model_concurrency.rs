// SPDX-License-Identifier: Apache-2.0

//! Per-Provider model slots shared by every Worker on one Device.
//! An OS file lock lives for the complete model call and is released on process exit.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::Path,
};

use rusqlite::OptionalExtension as _;
use sha2::{Digest as _, Sha256};
use winwincode_execution_port::generated::ModelOpenMessage;

use crate::{DeviceProviderError, DeviceProviderStore, device_model::validated_model_payload};

#[cfg(test)]
#[path = "device_model_concurrency_tests.rs"]
mod tests;

const PROVIDER_MODEL_SLOTS: usize = 3;

/// An acquired Device-wide Provider slot, or a request that cannot invoke a new model.
/// Moving this value into the model thread keeps the slot until that thread completes.
#[derive(Debug)]
pub struct DeviceModelPermit {
    _slot: Option<File>,
}

/// A full Provider queue leaves the original request unstarted and retryable.
#[derive(Debug)]
pub enum DeviceModelAdmission {
    Ready(DeviceModelPermit),
    Deferred,
}

impl DeviceProviderStore {
    /// Non-blocking admission before creating a first-start exchange record.
    /// Existing exchanges use their original replay/failure path without another slot.
    /// Malformed requests retain their existing bounded failure path.
    ///
    /// # Errors
    /// Returns storage, file permission or OS lock failures; full slots are `Deferred`.
    pub fn try_model_permit(
        &self,
        open: &ModelOpenMessage,
    ) -> Result<DeviceModelAdmission, DeviceProviderError> {
        let existing = self
            .connection
            .query_row(
                "SELECT 1 FROM exchanges WHERE exchange_id=?1",
                [&open.model_exchange_id.0],
                |_| Ok(()),
            )
            .optional()?;
        if existing.is_some() {
            return Ok(without_invocation());
        }
        let Ok(payload) = validated_model_payload(open) else {
            return Ok(without_invocation());
        };
        let Ok(request) = serde_json::from_slice::<serde_json::Value>(&payload) else {
            return Ok(without_invocation());
        };
        let Some(provider) = request.get("provider").and_then(serde_json::Value::as_str) else {
            return Ok(without_invocation());
        };
        if request
            .pointer("/request/model")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            return Ok(without_invocation());
        }
        if provider.trim().is_empty()
            || provider.trim() != provider
            || provider.len() > 128
            || provider.chars().any(char::is_control)
        {
            return Ok(without_invocation());
        }
        self.try_provider_model_permit(provider)
    }

    pub(crate) fn try_provider_model_permit(
        &self,
        provider: &str,
    ) -> Result<DeviceModelAdmission, DeviceProviderError> {
        let database = Path::new(self.connection.path().ok_or(DeviceProviderError)?);
        let root = database.parent().ok_or(DeviceProviderError)?;
        let slots = root.join("model-provider-slots");
        private_directory(&slots)?;
        let provider_slots = slots.join(format!("{:x}", Sha256::digest(provider.as_bytes())));
        private_directory(&provider_slots)?;
        for slot in 0..PROVIDER_MODEL_SLOTS {
            let file = private_slot_file(&provider_slots.join(format!("slot-{slot}")))?;
            match file.try_lock() {
                Ok(()) => {
                    return Ok(DeviceModelAdmission::Ready(DeviceModelPermit {
                        _slot: Some(file),
                    }));
                }
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(_)) => return Err(DeviceProviderError),
            }
        }
        Ok(DeviceModelAdmission::Deferred)
    }
}

fn without_invocation() -> DeviceModelAdmission {
    DeviceModelAdmission::Ready(DeviceModelPermit { _slot: None })
}

fn private_directory(path: &Path) -> Result<(), DeviceProviderError> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
        return Err(DeviceProviderError);
    }
    Ok(())
}

fn private_slot_file(path: &Path) -> Result<File, DeviceProviderError> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && (!meta.is_file() || meta.permissions().mode() & 0o077 != 0)
    {
        return Err(DeviceProviderError);
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
        return Err(DeviceProviderError);
    }
    // Keep the inode permanently: unlinking a live lock would create a second slot.
    Ok(file)
}
