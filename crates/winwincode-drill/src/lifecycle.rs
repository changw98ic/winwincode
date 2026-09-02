// SPDX-License-Identifier: Apache-2.0

use winwincode_audit::AuditScope;
use winwincode_domain::Sha256Digest;
use winwincode_storage::{ControlPlaneInstanceHealth, ControlPlaneInstanceState};

use crate::{DrillError, MAX_SAFE_INTEGER};

/// Closed instance lifecycle states supplied by the single deployment
/// health/drain authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    Healthy,
    Draining,
    Drained,
    Unhealthy,
}

/// One exact health/drain observation. It contains no business payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPlaneLifecycleSnapshot {
    scope: AuditScope,
    release_source_digest: Sha256Digest,
    state: LifecycleState,
    accepting_new_work: bool,
    in_flight_requests: u64,
    confirmed_sequence: u64,
    confirmed_state_digest: Sha256Digest,
    observed_at_millis: u64,
}

impl ControlPlaneLifecycleSnapshot {
    /// Builds one internally consistent authority observation.
    ///
    /// # Errors
    ///
    /// Rejects malformed digests/time/counts and invalid state combinations.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        scope: AuditScope,
        release_source_digest: Sha256Digest,
        state: LifecycleState,
        accepting_new_work: bool,
        in_flight_requests: u64,
        confirmed_sequence: u64,
        confirmed_state_digest: Sha256Digest,
        observed_at_millis: u64,
    ) -> Result<Self, DrillError> {
        validate_scope(&scope)?;
        validate_digest(&release_source_digest)?;
        validate_digest(&confirmed_state_digest)?;
        if observed_at_millis == 0
            || observed_at_millis > MAX_SAFE_INTEGER
            || in_flight_requests > MAX_SAFE_INTEGER
            || confirmed_sequence > MAX_SAFE_INTEGER
        {
            return Err(DrillError::invalid());
        }
        let valid_state = match state {
            LifecycleState::Healthy => true,
            LifecycleState::Draining | LifecycleState::Unhealthy => !accepting_new_work,
            LifecycleState::Drained => !accepting_new_work && in_flight_requests == 0,
        };
        if !valid_state {
            return Err(DrillError::invalid());
        }
        Ok(Self {
            scope,
            release_source_digest,
            state,
            accepting_new_work,
            in_flight_requests,
            confirmed_sequence,
            confirmed_state_digest,
            observed_at_millis,
        })
    }

    /// Maps the canonical Control Plane instance health cut into one drill
    /// observation. The caller supplies only deployment facts that the
    /// instance ledger does not own.
    ///
    /// # Errors
    ///
    /// Rejects an invalid tenant/release fact, an inconsistent lease or
    /// admission projection, or unsafe values. An expired unfinished drain is
    /// mapped to [`LifecycleState::Unhealthy`].
    pub fn from_instance_health(
        scope: AuditScope,
        release_source_digest: Sha256Digest,
        observed_at_millis: u64,
        health: &ControlPlaneInstanceHealth,
    ) -> Result<Self, DrillError> {
        let durable = health.state;
        let expected_lease_valid = health.lease_expires_at > observed_at_millis
            && !matches!(
                durable,
                ControlPlaneInstanceState::Fenced | ControlPlaneInstanceState::Closed
            );
        let expected_admission =
            expected_lease_valid && durable == ControlPlaneInstanceState::Active;
        let drain_shape_is_valid = match durable {
            ControlPlaneInstanceState::Active => health.drain_deadline_at.is_none(),
            ControlPlaneInstanceState::Draining => health.drain_deadline_at.is_some(),
            ControlPlaneInstanceState::Fenced | ControlPlaneInstanceState::Closed => true,
        };
        if health.lease_expires_at == 0
            || health.lease_expires_at > MAX_SAFE_INTEGER
            || health
                .drain_deadline_at
                .is_some_and(|deadline| deadline == 0 || deadline > MAX_SAFE_INTEGER)
            || health.lease_valid != expected_lease_valid
            || health.accepting_new_work != expected_admission
            || !drain_shape_is_valid
        {
            return Err(DrillError::invalid());
        }
        let state = match durable {
            ControlPlaneInstanceState::Active if expected_lease_valid => LifecycleState::Healthy,
            ControlPlaneInstanceState::Draining
                if expected_lease_valid && health.in_flight == 0 =>
            {
                LifecycleState::Drained
            }
            ControlPlaneInstanceState::Draining
                if expected_lease_valid
                    && health
                        .drain_deadline_at
                        .is_some_and(|deadline| deadline > observed_at_millis) =>
            {
                LifecycleState::Draining
            }
            ControlPlaneInstanceState::Active
            | ControlPlaneInstanceState::Draining
            | ControlPlaneInstanceState::Fenced
            | ControlPlaneInstanceState::Closed => LifecycleState::Unhealthy,
        };
        Self::try_new(
            scope,
            release_source_digest,
            state,
            health.accepting_new_work,
            health.in_flight,
            health.confirmed_state_sequence,
            health.confirmed_state_digest.clone(),
            observed_at_millis,
        )
    }

    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }
    #[must_use]
    pub const fn release_source_digest(&self) -> &Sha256Digest {
        &self.release_source_digest
    }
    #[must_use]
    pub const fn state(&self) -> LifecycleState {
        self.state
    }
    #[must_use]
    pub const fn accepting_new_work(&self) -> bool {
        self.accepting_new_work
    }
    #[must_use]
    pub const fn in_flight_requests(&self) -> u64 {
        self.in_flight_requests
    }
    #[must_use]
    pub const fn confirmed_sequence(&self) -> u64 {
        self.confirmed_sequence
    }
    #[must_use]
    pub const fn confirmed_state_digest(&self) -> &Sha256Digest {
        &self.confirmed_state_digest
    }
    #[must_use]
    pub const fn observed_at_millis(&self) -> u64 {
        self.observed_at_millis
    }
}

/// Stable health/drain authority failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrainAuthorityError;

impl DrainAuthorityError {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for DrainAuthorityError {
    fn default() -> Self {
        Self::new()
    }
}

/// Narrow consumer port for the one Control Plane instance health/drain
/// authority. Implementations keep lifecycle state outside this crate.
pub trait DeploymentDrainAuthority {
    /// Reads current instance readiness and confirmed state.
    ///
    /// # Errors
    ///
    /// Returns when the single lifecycle authority is unavailable.
    fn preflight(
        &mut self,
        scope: &AuditScope,
    ) -> Result<ControlPlaneLifecycleSnapshot, DrainAuthorityError>;

    /// Stops admission of new work.
    ///
    /// # Errors
    ///
    /// Returns when drain cannot be durably started.
    fn begin_drain(
        &mut self,
        scope: &AuditScope,
    ) -> Result<ControlPlaneLifecycleSnapshot, DrainAuthorityError>;

    /// Reads the zero-in-flight drained boundary.
    ///
    /// # Errors
    ///
    /// Returns when drain completion cannot be observed.
    fn await_drained(
        &mut self,
        scope: &AuditScope,
    ) -> Result<ControlPlaneLifecycleSnapshot, DrainAuthorityError>;

    /// Reads target health before it accepts traffic.
    ///
    /// # Errors
    ///
    /// Returns when target health is unavailable.
    fn target_health(
        &mut self,
        scope: &AuditScope,
        release_source_digest: &Sha256Digest,
    ) -> Result<ControlPlaneLifecycleSnapshot, DrainAuthorityError>;

    /// Resumes admission for the exact active release.
    ///
    /// # Errors
    ///
    /// Returns when traffic admission cannot be durably resumed.
    fn resume(
        &mut self,
        scope: &AuditScope,
        release_source_digest: &Sha256Digest,
    ) -> Result<ControlPlaneLifecycleSnapshot, DrainAuthorityError>;
}

pub(crate) fn validate_digest(digest: &Sha256Digest) -> Result<(), DrillError> {
    let valid = digest.0.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    });
    if valid {
        Ok(())
    } else {
        Err(DrillError::invalid())
    }
}

pub(crate) fn validate_scope(scope: &AuditScope) -> Result<(), DrillError> {
    let canonical = |value: &str, prefix: &str| {
        const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        value
            .strip_prefix(&format!("{prefix}_"))
            .is_some_and(|suffix| {
                suffix.len() == 26 && suffix.bytes().all(|byte| CROCKFORD.contains(&byte))
            })
    };
    let valid = match scope {
        AuditScope::Organization { organization_id } => canonical(&organization_id.0, "org"),
        AuditScope::Workspace {
            organization_id,
            workspace_id,
        } => canonical(&organization_id.0, "org") && canonical(&workspace_id.0, "wsp"),
        AuditScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => {
            canonical(&organization_id.0, "org")
                && canonical(&workspace_id.0, "wsp")
                && canonical(&project_id.0, "prj")
        }
        AuditScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => {
            canonical(&organization_id.0, "org")
                && canonical(&workspace_id.0, "wsp")
                && canonical(&project_id.0, "prj")
                && canonical(&repository_id.0, "rep")
        }
    };
    if valid {
        Ok(())
    } else {
        Err(DrillError::invalid())
    }
}
