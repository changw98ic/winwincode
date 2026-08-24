// SPDX-License-Identifier: Apache-2.0

//! Delivery application services.
//!
//! These modules decide product facts and return immutable effects. They do
//! not run Codex, schedule its internal work, or publish before persistence.

use std::{error::Error, fmt};

pub mod attention;
pub mod session_binding;
pub mod stage;
pub mod task;

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
