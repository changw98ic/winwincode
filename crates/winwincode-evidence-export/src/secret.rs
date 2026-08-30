// SPDX-License-Identifier: Apache-2.0

use crate::{EvidenceError, EvidenceErrorKind};

const SECRET_MARKERS: &[&str] = &[
    "authorization:bearer",
    "aws_secret_access_key",
    "client_secret",
    "credential_locator",
    "-----beginprivatekey",
    "ghp_",
    "password=",
    "\"password\":",
    "private_key",
    "secret=",
    "\"secret\":",
    "sk_live_",
    "wwc_session=",
];

pub(crate) struct SecretScanner {
    label: String,
    tail: Vec<u8>,
}

impl SecretScanner {
    pub(crate) fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            tail: Vec::new(),
        }
    }

    pub(crate) fn inspect(&mut self, bytes: &[u8]) -> Result<(), EvidenceError> {
        let mut normalized = Vec::with_capacity(self.tail.len() + bytes.len());
        normalized.extend_from_slice(&self.tail);
        normalized.extend(bytes.iter().filter_map(|byte| match byte {
            b' ' | b'\t' => None,
            _ => Some(byte.to_ascii_lowercase()),
        }));
        if SECRET_MARKERS
            .iter()
            .any(|marker| contains_bytes(&normalized, marker.as_bytes()))
        {
            return Err(EvidenceError::new(
                EvidenceErrorKind::SecretDetected,
                format!("secret-like content detected in {}", self.label),
            ));
        }
        let keep = SECRET_MARKERS
            .iter()
            .map(|marker| marker.len())
            .max()
            .unwrap_or(1)
            .saturating_sub(1)
            .min(normalized.len());
        self.tail.clear();
        self.tail
            .extend_from_slice(&normalized[normalized.len() - keep..]);
        Ok(())
    }
}

pub(crate) fn reject_secret_bytes(label: &str, bytes: &[u8]) -> Result<(), EvidenceError> {
    SecretScanner::new(label).inspect(bytes)
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
