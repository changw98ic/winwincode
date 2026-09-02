// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::DrillError;

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Stable identity for one exact upgrade or recovery drill.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct DrillId(String);

impl DrillId {
    /// Builds canonical `drl_` plus 26 Crockford characters.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities.
    pub fn try_new(value: impl Into<String>) -> Result<Self, DrillError> {
        let value = value.into();
        if !Self::canonical(&value) {
            return Err(DrillError::invalid());
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn validate(&self) -> Result<(), DrillError> {
        if Self::canonical(&self.0) {
            Ok(())
        } else {
            Err(DrillError::invalid())
        }
    }

    fn canonical(value: &str) -> bool {
        value.strip_prefix("drl_").is_some_and(|suffix| {
            suffix.len() == 26 && suffix.bytes().all(|byte| CROCKFORD.contains(&byte))
        })
    }
}
