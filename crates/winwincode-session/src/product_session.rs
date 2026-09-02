// SPDX-License-Identifier: Apache-2.0

//! `ProductSession`'s lifecycle state machine.

use std::fmt;

use serde::{Deserialize, Serialize};
use winwincode_domain::{Instant, ProductSessionId, ProjectId, RepositoryId};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_TITLE_LENGTH: usize = 500;
const MAX_REASON_LENGTH: usize = 2_000;

/// The product-visible state of one `ProductSession`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductSessionState {
    Idle,
    Running,
    WaitingForInput,
    WaitingForApproval,
    Cancelled,
    Closed,
    Failed,
}

/// Input owned by the Control Plane when creating one `ProductSession`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductSessionCreate {
    pub product_session_id: ProductSessionId,
    pub project_id: ProjectId,
    pub repository_id: RepositoryId,
    pub title: String,
    pub now: Instant,
}

/// `ProductSession` aggregate. Mutations go through the lifecycle methods so a
/// caller cannot skip a state transition or revision update.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProductSession {
    product_session_id: ProductSessionId,
    project_id: ProjectId,
    repository_id: RepositoryId,
    title: String,
    state: ProductSessionState,
    revision: u64,
    updated_at: Instant,
}

/// `ProductSession` lifecycle failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductSessionError {
    InvalidIdentity(&'static str),
    InvalidTitle,
    InvalidReason,
    InvalidInstant,
    InvalidTransition {
        from: ProductSessionState,
        operation: &'static str,
    },
    RevisionOverflow,
}

impl fmt::Display for ProductSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity(field) => {
                write!(formatter, "invalid ProductSession identity: {field}")
            }
            Self::InvalidTitle => formatter.write_str("ProductSession title is invalid"),
            Self::InvalidReason => formatter.write_str("ProductSession reason is invalid"),
            Self::InvalidInstant => formatter.write_str("ProductSession instant is invalid"),
            Self::InvalidTransition { from, operation } => {
                write!(
                    formatter,
                    "cannot {operation} from ProductSession state {from:?}"
                )
            }
            Self::RevisionOverflow => formatter.write_str("ProductSession revision overflowed"),
        }
    }
}

impl std::error::Error for ProductSessionError {}

#[allow(clippy::missing_errors_doc)]
impl ProductSession {
    /// Creates an idle `ProductSession` at revision one.
    pub fn create(input: ProductSessionCreate) -> Result<Self, ProductSessionError> {
        validate_id(&input.product_session_id.0, "productSessionId", "psn_")?;
        validate_id(&input.project_id.0, "projectId", "prj_")?;
        validate_id(&input.repository_id.0, "repositoryId", "rep_")?;
        validate_title(&input.title)?;
        validate_instant(&input.now)?;
        Ok(Self {
            product_session_id: input.product_session_id,
            project_id: input.project_id,
            repository_id: input.repository_id,
            title: input.title,
            state: ProductSessionState::Idle,
            revision: 1,
            updated_at: input.now,
        })
    }

    /// Convenience constructor for callers that do not need the input struct.
    pub fn new(
        product_session_id: ProductSessionId,
        project_id: ProjectId,
        repository_id: RepositoryId,
        title: impl Into<String>,
        now: Instant,
    ) -> Result<Self, ProductSessionError> {
        Self::create(ProductSessionCreate {
            product_session_id,
            project_id,
            repository_id,
            title: title.into(),
            now,
        })
    }

    #[must_use]
    pub const fn id(&self) -> &ProductSessionId {
        &self.product_session_id
    }

    #[must_use]
    pub const fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    #[must_use]
    pub const fn repository_id(&self) -> &RepositoryId {
        &self.repository_id
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub const fn state(&self) -> ProductSessionState {
        self.state
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn updated_at(&self) -> &Instant {
        &self.updated_at
    }

    /// Starts a Chat turn from an idle or failed session.
    pub fn begin_turn(&mut self, now: Instant) -> Result<(), ProductSessionError> {
        self.transition(ProductSessionState::Running, "begin_turn", now, |state| {
            matches!(
                state,
                ProductSessionState::Idle | ProductSessionState::Failed
            )
        })
    }

    /// Moves a running turn to the user-input wait state.
    pub fn wait_for_input(&mut self, now: Instant) -> Result<(), ProductSessionError> {
        self.transition(
            ProductSessionState::WaitingForInput,
            "wait_for_input",
            now,
            |state| *state == ProductSessionState::Running,
        )
    }

    /// Moves a running turn to the execution-approval wait state.
    pub fn wait_for_approval(&mut self, now: Instant) -> Result<(), ProductSessionError> {
        self.transition(
            ProductSessionState::WaitingForApproval,
            "wait_for_approval",
            now,
            |state| *state == ProductSessionState::Running,
        )
    }

    /// Resumes a turn after its input or approval request is resolved.
    pub fn resume(&mut self, now: Instant) -> Result<(), ProductSessionError> {
        self.transition(ProductSessionState::Running, "resume", now, |state| {
            matches!(
                state,
                ProductSessionState::WaitingForInput | ProductSessionState::WaitingForApproval
            )
        })
    }

    /// Completes the current turn and returns the session to idle.
    pub fn complete_turn(&mut self, now: Instant) -> Result<(), ProductSessionError> {
        self.transition(ProductSessionState::Idle, "complete_turn", now, |state| {
            *state == ProductSessionState::Running
        })
    }

    /// Records a terminal model or infrastructure failure for the current turn.
    pub fn fail(
        &mut self,
        reason: impl AsRef<str>,
        now: Instant,
    ) -> Result<(), ProductSessionError> {
        validate_reason(reason.as_ref())?;
        self.transition(ProductSessionState::Failed, "fail", now, |state| {
            matches!(
                state,
                ProductSessionState::Running
                    | ProductSessionState::WaitingForInput
                    | ProductSessionState::WaitingForApproval
            )
        })
    }

    /// Cancels a session. Replaying cancellation is idempotent.
    pub fn cancel(
        &mut self,
        reason: impl AsRef<str>,
        now: Instant,
    ) -> Result<(), ProductSessionError> {
        validate_reason(reason.as_ref())?;
        if self.state == ProductSessionState::Cancelled {
            return Ok(());
        }
        self.transition(ProductSessionState::Cancelled, "cancel", now, |state| {
            !matches!(state, ProductSessionState::Closed)
        })
    }

    /// Closes an idle, failed, or cancelled session. Replaying close is idempotent.
    pub fn close(&mut self, now: Instant) -> Result<(), ProductSessionError> {
        if self.state == ProductSessionState::Closed {
            return Ok(());
        }
        self.transition(ProductSessionState::Closed, "close", now, |state| {
            matches!(
                state,
                ProductSessionState::Idle
                    | ProductSessionState::Cancelled
                    | ProductSessionState::Failed
            )
        })
    }

    fn transition(
        &mut self,
        next: ProductSessionState,
        operation: &'static str,
        now: Instant,
        allowed: impl FnOnce(&ProductSessionState) -> bool,
    ) -> Result<(), ProductSessionError> {
        validate_instant(&now)?;
        if !allowed(&self.state) {
            return Err(ProductSessionError::InvalidTransition {
                from: self.state,
                operation,
            });
        }
        self.revision = self
            .revision
            .checked_add(1)
            .filter(|revision| *revision <= MAX_SAFE_INTEGER)
            .ok_or(ProductSessionError::RevisionOverflow)?;
        self.state = next;
        self.updated_at = now;
        Ok(())
    }
}

fn validate_title(title: &str) -> Result<(), ProductSessionError> {
    if title.is_empty() || title.chars().count() > MAX_TITLE_LENGTH {
        return Err(ProductSessionError::InvalidTitle);
    }
    Ok(())
}

fn validate_reason(reason: &str) -> Result<(), ProductSessionError> {
    if reason.is_empty() || reason.chars().count() > MAX_REASON_LENGTH {
        return Err(ProductSessionError::InvalidReason);
    }
    Ok(())
}

fn validate_id(value: &str, field: &'static str, prefix: &str) -> Result<(), ProductSessionError> {
    if !canonical_id(value, prefix) {
        return Err(ProductSessionError::InvalidIdentity(field));
    }
    Ok(())
}

fn canonical_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H'
                            | b'J'..=b'K'
                            | b'M'..=b'N'
                            | b'P'..=b'T'
                            | b'V'..=b'Z'
                    )
            })
    })
}

fn validate_instant(instant: &Instant) -> Result<(), ProductSessionError> {
    let value = instant.0.as_bytes();
    // `YYYY-MM-DDTHH:mm:ss.sssZ`, matching the canonical schema's fixed UTC form.
    let valid_shape = value.len() == 24
        && value[4] == b'-'
        && value[7] == b'-'
        && value[10] == b'T'
        && value[13] == b':'
        && value[16] == b':'
        && value[19] == b'.'
        && value[23] == b'Z'
        && value.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        });
    if !valid_shape {
        return Err(ProductSessionError::InvalidInstant);
    }
    Ok(())
}

impl<'de> Deserialize<'de> for ProductSession {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Wire {
            product_session_id: ProductSessionId,
            project_id: ProjectId,
            repository_id: RepositoryId,
            title: String,
            state: ProductSessionState,
            revision: u64,
            updated_at: Instant,
        }

        let wire = Wire::deserialize(deserializer)?;
        validate_id(&wire.product_session_id.0, "productSessionId", "psn_")
            .map_err(serde::de::Error::custom)?;
        validate_id(&wire.project_id.0, "projectId", "prj_").map_err(serde::de::Error::custom)?;
        validate_id(&wire.repository_id.0, "repositoryId", "rep_")
            .map_err(serde::de::Error::custom)?;
        validate_title(&wire.title).map_err(serde::de::Error::custom)?;
        validate_instant(&wire.updated_at).map_err(serde::de::Error::custom)?;
        if wire.revision == 0 || wire.revision > MAX_SAFE_INTEGER {
            return Err(serde::de::Error::custom(
                "ProductSession revision is invalid",
            ));
        }
        Ok(Self {
            product_session_id: wire.product_session_id,
            project_id: wire.project_id,
            repository_id: wire.repository_id,
            title: wire.title,
            state: wire.state,
            revision: wire.revision,
            updated_at: wire.updated_at,
        })
    }
}
