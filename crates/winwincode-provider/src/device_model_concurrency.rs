// SPDX-License-Identifier: Apache-2.0

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::Path,
};

use rusqlite::OptionalExtension as _;
use sha2::{Digest as _, Sha256};
use winwincode_execution_port::generated::ModelOpenMessage;

use crate::{DeviceProviderError, ProviderRuntimeStore, device_model::validated_model_payload};

#[cfg(test)]
#[path = "device_model_concurrency_tests.rs"]
mod tests;

const PROVIDER_MODEL_SLOTS: usize = 3;

#[derive(Debug)]
pub(crate) struct DeviceModelExchangeOwner {
    file: File,
}

impl Drop for DeviceModelExchangeOwner {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// An acquired Device-wide Provider slot for exactly one physical attempt.
#[derive(Debug)]
pub struct DeviceModelPermit {
    slot: File,
}

impl Drop for DeviceModelPermit {
    fn drop(&mut self) {
        // The invocation owner returns its slot before inherited aliases close.
        let _ = self.slot.unlock();
    }
}

#[derive(Debug)]
pub enum DeviceModelAdmission {
    InvokeAttempt(DeviceModelPermit),
    ReplayStoredResult,
    RecoverExchange,
    RejectRequest,
    Deferred,
}

impl ProviderRuntimeStore {
    pub(crate) fn try_model_exchange_owner(
        &self,
        exchange_id: &str,
    ) -> Result<Option<DeviceModelExchangeOwner>, DeviceProviderError> {
        let database = Path::new(
            self.connection
                .path()
                .ok_or(DeviceProviderError::StateInvalid)?,
        );
        let root = database
            .parent()
            .ok_or(DeviceProviderError::StateInvalid)?
            .join("model-exchange-owners");
        private_directory(&root)?;
        let file =
            private_slot_file(&root.join(format!("{:x}", Sha256::digest(exchange_id.as_bytes()))))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(DeviceModelExchangeOwner { file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(_)) => Err(DeviceProviderError::FilesystemUnavailable),
        }
    }
    /// Stored replay and recovery carry no physical invocation authority.
    pub fn try_model_permit(
        &self,
        open: &ModelOpenMessage,
    ) -> Result<DeviceModelAdmission, DeviceProviderError> {
        let existing = self
            .connection
            .query_row(
                "SELECT chunks IS NOT NULL FROM exchanges WHERE exchange_id=?1",
                [&open.model_exchange_id.0],
                |row| row.get::<_, bool>(0),
            )
            .optional()?;
        if let Some(completed) = existing {
            return Ok(if completed {
                DeviceModelAdmission::ReplayStoredResult
            } else {
                DeviceModelAdmission::RecoverExchange
            });
        }
        self.try_model_attempt_permit(open)
    }

    pub fn try_model_attempt_permit(
        &self,
        open: &ModelOpenMessage,
    ) -> Result<DeviceModelAdmission, DeviceProviderError> {
        let Ok(payload) = validated_model_payload(open) else {
            return Ok(DeviceModelAdmission::RejectRequest);
        };
        let Ok(request) = serde_json::from_slice::<serde_json::Value>(&payload) else {
            return Ok(DeviceModelAdmission::RejectRequest);
        };
        let Some(provider) = request.get("provider").and_then(serde_json::Value::as_str) else {
            return Ok(DeviceModelAdmission::RejectRequest);
        };
        if request
            .pointer("/request/model")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            return Ok(DeviceModelAdmission::RejectRequest);
        }
        if provider.trim().is_empty()
            || provider.trim() != provider
            || provider.len() > 128
            || provider.chars().any(char::is_control)
        {
            return Ok(DeviceModelAdmission::RejectRequest);
        }
        self.try_provider_model_permit(provider)
    }

    pub(crate) fn try_provider_model_permit(
        &self,
        provider: &str,
    ) -> Result<DeviceModelAdmission, DeviceProviderError> {
        let database = Path::new(
            self.connection
                .path()
                .ok_or(DeviceProviderError::StateInvalid)?,
        );
        let root = database.parent().ok_or(DeviceProviderError::StateInvalid)?;
        let slots = root.join("model-provider-slots");
        private_directory(&slots)?;
        let provider_slots = slots.join(format!("{:x}", Sha256::digest(provider.as_bytes())));
        private_directory(&provider_slots)?;
        for slot in 0..PROVIDER_MODEL_SLOTS {
            let file = private_slot_file(&provider_slots.join(format!("slot-{slot}")))?;
            match file.try_lock() {
                Ok(()) => {
                    return Ok(DeviceModelAdmission::InvokeAttempt(DeviceModelPermit {
                        slot: file,
                    }));
                }
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(_)) => {
                    return Err(DeviceProviderError::FilesystemUnavailable);
                }
            }
        }
        Ok(DeviceModelAdmission::Deferred)
    }
}

pub(crate) fn private_directory(path: &Path) -> Result<(), DeviceProviderError> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
        return Err(DeviceProviderError::StateInvalid);
    }
    Ok(())
}

pub(crate) fn private_slot_file(path: &Path) -> Result<File, DeviceProviderError> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && (!meta.is_file() || meta.permissions().mode() & 0o077 != 0)
    {
        return Err(DeviceProviderError::StateInvalid);
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
        return Err(DeviceProviderError::StateInvalid);
    }
    Ok(file)
}
