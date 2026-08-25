// SPDX-License-Identifier: Apache-2.0

//! Delivery application services.
//!
//! These modules decide product facts and return immutable effects. They do
//! not run Codex, schedule its internal work, or publish before persistence.

use std::{error::Error, fmt};

use crate::domain::{Delivery, MAX_SAFE_INTEGER};

pub mod attention;
pub mod session_binding;
#[cfg(any(test, feature = "test-support"))]
pub mod solution_review;
#[cfg(not(any(test, feature = "test-support")))]
pub(crate) mod solution_review;
pub mod stage;
pub mod task;
pub mod task_breakdown;
pub mod verdict;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinationErrorCode {
    InvalidRequest,
    RevisionConflict,
    WrongState,
    Conflict,
    AttentionRequired,
    BindingConflict,
    StaleAttention,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinationError {
    code: CoordinationErrorCode,
    message: String,
}

impl CoordinationError {
    pub(crate) fn new(code: CoordinationErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub const fn code(&self) -> CoordinationErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CoordinationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CoordinationError {}

pub(crate) fn require_mutation_time(
    delivery: &Delivery,
    now_millis: u64,
) -> Result<(), CoordinationError> {
    if now_millis < delivery.snapshot().updated_at_millis || now_millis > MAX_SAFE_INTEGER {
        Err(CoordinationError::new(
            CoordinationErrorCode::InvalidRequest,
            "mutation time must not precede the current Delivery state",
        ))
    } else {
        Ok(())
    }
}
