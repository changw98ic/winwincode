// SPDX-License-Identifier: Apache-2.0

//! The bounded, canonical contract for ephemeral `DebugProbe` experiments.
//!
//! The wire types in [`generated`] describe the durable experiment record.  This
//! module owns the parts that cannot be expressed by JSON Schema: the small DSL,
//! its digest, and the lifecycle transition table.  In particular, an
//! experiment is never a candidate and its workspace must be cleaned before it
//! can be reused.

use std::{fmt, path::Path};

use sha2::{Digest as _, Sha256};
use winwincode_domain::Sha256Digest;

use crate::generated::DebugExperimentStatus;

const DSL_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-experiment.dsl.v1\0";
const MAX_OPERATIONS: usize = 16;
const MAX_PATH_BYTES: usize = 512;
const MAX_SYMBOL_BYTES: usize = 256;
const MAX_EXPRESSION_BYTES: usize = 2_048;
const MAX_ARG_BYTES: usize = 4_096;
const MAX_REPETITIONS: u32 = 100;

/// One deliberately small instrumentation operation.  Operations are data,
/// not shell snippets; the Worker turns them into an admitted Action Gateway
/// request after the workspace barrier has been acquired.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstrumentationOperation {
    /// Add a bounded source-level log probe at a known symbol.
    InsertLogProbe {
        path: String,
        symbol: String,
        expression: String,
    },
    /// Run one declared test command a bounded number of times.
    RepeatTest {
        path: String,
        command: Vec<String>,
        repetitions: u32,
    },
}

/// User-supplied experiment program before semantic validation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstrumentationDsl {
    pub operations: Vec<InstrumentationOperation>,
}

/// A DSL that passed all host bounds and has a content-derived identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedInstrumentationDsl {
    dsl: InstrumentationDsl,
    digest: Sha256Digest,
}

impl ValidatedInstrumentationDsl {
    /// Returns the exact validated operations.
    #[must_use]
    pub const fn dsl(&self) -> &InstrumentationDsl {
        &self.dsl
    }

    /// Returns the digest bound into the durable experiment record.
    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }

    /// Returns the validated operation sequence.
    #[must_use]
    pub fn operations(&self) -> &[InstrumentationOperation] {
        &self.dsl.operations
    }
}

/// Stable semantic failure; input values are intentionally not echoed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugExperimentErrorCode {
    InvalidDsl,
    InvalidTransition,
    InvalidTtl,
    StaleRevision,
    CleanupRequired,
}

/// Error returned by contract validation and lifecycle helpers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DebugExperimentError {
    code: DebugExperimentErrorCode,
    message: &'static str,
}

impl DebugExperimentError {
    const fn new(code: DebugExperimentErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    /// Machine-readable failure category.
    #[must_use]
    pub const fn code(self) -> DebugExperimentErrorCode {
        self.code
    }
}

impl fmt::Display for DebugExperimentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for DebugExperimentError {}

/// Validates and seals an experiment DSL.
///
/// # Errors
///
/// Returns [`DebugExperimentErrorCode::InvalidDsl`] when an operation, path,
/// expression, command, or repetition exceeds the bounded contract.
pub fn validate_instrumentation_dsl(
    dsl: InstrumentationDsl,
) -> Result<ValidatedInstrumentationDsl, DebugExperimentError> {
    if dsl.operations.is_empty() || dsl.operations.len() > MAX_OPERATIONS {
        return Err(invalid_dsl());
    }
    for operation in &dsl.operations {
        match operation {
            InstrumentationOperation::InsertLogProbe {
                path,
                symbol,
                expression,
            } => {
                validate_relative_path(path)?;
                validate_symbol(symbol)?;
                validate_expression(expression)?;
            }
            InstrumentationOperation::RepeatTest {
                path,
                command,
                repetitions,
            } => {
                validate_relative_path(path)?;
                if !(1..=MAX_REPETITIONS).contains(repetitions) || command.is_empty() {
                    return Err(invalid_dsl());
                }
                let mut total = 0_usize;
                for argument in command {
                    if argument.is_empty()
                        || argument.len() > MAX_ARG_BYTES
                        || argument.contains('\0')
                        || argument.chars().any(char::is_control)
                        || argument.contains([';', '|', '&', '$', '`', '\n', '\r'])
                    {
                        return Err(invalid_dsl());
                    }
                    total = total.checked_add(argument.len()).ok_or_else(invalid_dsl)?;
                }
                if total > MAX_ARG_BYTES || command.len() > 64 {
                    return Err(invalid_dsl());
                }
            }
        }
    }
    let bytes = serde_json::to_vec(&dsl).map_err(|_| invalid_dsl())?;
    let mut hasher = Sha256::new();
    hasher.update(DSL_DIGEST_DOMAIN);
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    Ok(ValidatedInstrumentationDsl {
        dsl,
        digest: Sha256Digest(format!("sha256:{:x}", hasher.finalize())),
    })
}

/// Validates the TTL relation without interpreting wall-clock strings.  The
/// canonical Instant format is UTC RFC3339, whose fixed-width representation
/// sorts chronologically when both values are normalized by the producer.
///
/// # Errors
///
/// Returns [`DebugExperimentErrorCode::InvalidTtl`] when expiry is not later
/// than creation.
pub fn validate_ttl(created_at: &str, expires_at: &str) -> Result<(), DebugExperimentError> {
    if created_at.is_empty() || expires_at.is_empty() || expires_at <= created_at {
        return Err(DebugExperimentError::new(
            DebugExperimentErrorCode::InvalidTtl,
            "experiment expiry must be after creation",
        ));
    }
    Ok(())
}

/// Returns the only legal next state for one lifecycle event.
///
/// # Errors
///
/// Returns [`DebugExperimentErrorCode::InvalidTransition`] for any transition
/// outside the canonical lifecycle table.
pub fn transition_experiment(
    current: &DebugExperimentStatus,
    next: DebugExperimentStatus,
) -> Result<DebugExperimentStatus, DebugExperimentError> {
    use DebugExperimentStatus as S;
    let valid = matches!(
        (current, &next),
        (
            S::Planned,
            S::Preparing | S::CleanupPending | S::Cancelled | S::Expired
        ) | (
            S::Preparing | S::Running,
            S::Ready | S::CleanupPending | S::Cancelled | S::Expired
        ) | (
            S::Ready,
            S::Running | S::CleanupPending | S::Cancelled | S::Expired
        ) | (S::Cancelled | S::Expired, S::CleanupPending)
            | (S::CleanupPending, S::CleanedUp | S::CleanupFailed)
    );
    if valid {
        Ok(next)
    } else {
        Err(DebugExperimentError::new(
            DebugExperimentErrorCode::InvalidTransition,
            "experiment lifecycle transition is not permitted",
        ))
    }
}

/// Checks that a terminal cleanup receipt is sufficient to reuse the slot.
pub fn cleanup_allows_reuse(status: &DebugExperimentStatus, residual_resources: u32) -> bool {
    matches!(status, DebugExperimentStatus::CleanedUp) && residual_resources == 0
}

fn validate_relative_path(value: &str) -> Result<(), DebugExperimentError> {
    if value.is_empty()
        || value.len() > MAX_PATH_BYTES
        || value.contains('\0')
        || value.contains('\\')
        || Path::new(value).is_absolute()
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || value.chars().any(char::is_control)
    {
        return Err(invalid_dsl());
    }
    Ok(())
}

fn validate_symbol(value: &str) -> Result<(), DebugExperimentError> {
    if value.is_empty()
        || value.len() > MAX_SYMBOL_BYTES
        || !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || byte == b'_' || (index > 0 && byte == b':')
        })
    {
        return Err(invalid_dsl());
    }
    Ok(())
}

fn validate_expression(value: &str) -> Result<(), DebugExperimentError> {
    if value.is_empty()
        || value.len() > MAX_EXPRESSION_BYTES
        || value.contains('\0')
        || value.chars().any(char::is_control)
        || value.contains([';', '|', '&', '$', '`', '\n', '\r'])
    {
        return Err(invalid_dsl());
    }
    Ok(())
}

fn invalid_dsl() -> DebugExperimentError {
    DebugExperimentError::new(
        DebugExperimentErrorCode::InvalidDsl,
        "instrumentation DSL exceeds its bounded host contract",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dsl() -> InstrumentationDsl {
        InstrumentationDsl {
            operations: vec![
                InstrumentationOperation::InsertLogProbe {
                    path: "src/lib.rs".into(),
                    symbol: "module::function".into(),
                    expression: "value.len()".into(),
                },
                InstrumentationOperation::RepeatTest {
                    path: "tests/smoke.rs".into(),
                    command: vec!["cargo".into(), "test".into(), "smoke".into()],
                    repetitions: 2,
                },
            ],
        }
    }

    #[test]
    fn seals_dsl_deterministically_and_rejects_boundaries() {
        let first = validate_instrumentation_dsl(dsl()).expect("valid DSL");
        let second = validate_instrumentation_dsl(dsl()).expect("valid DSL");
        assert_eq!(first.digest(), second.digest());
        let mut invalid = dsl();
        if let InstrumentationOperation::RepeatTest { path, .. } = &mut invalid.operations[1] {
            *path = "../escape".into();
        }
        assert_eq!(
            validate_instrumentation_dsl(invalid).unwrap_err().code(),
            DebugExperimentErrorCode::InvalidDsl
        );
    }

    #[test]
    fn lifecycle_and_cleanup_are_strict() {
        assert!(
            transition_experiment(
                &DebugExperimentStatus::Planned,
                DebugExperimentStatus::Preparing
            )
            .is_ok()
        );
        assert!(
            transition_experiment(
                &DebugExperimentStatus::Planned,
                DebugExperimentStatus::Ready
            )
            .is_err()
        );
        assert!(cleanup_allows_reuse(&DebugExperimentStatus::CleanedUp, 0));
        assert!(!cleanup_allows_reuse(&DebugExperimentStatus::CleanedUp, 1));
        assert!(validate_ttl("2026-09-07T00:00:00Z", "2026-09-07T00:00:01Z").is_ok());
    }
}
