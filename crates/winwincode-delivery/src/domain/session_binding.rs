// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use winwincode_domain::{DeliveryId, StageRunId};

use super::{
    DeliveryValidationError, DeliveryValidationErrorCode, SessionBindingId, portable_identifier,
    safe_non_negative, schema_version, validation_error,
};

/// Current TypeScript session identities kept only as migration input.
///
/// The ProductSession/WorkerSession/CodexThread split replaces these fields in
/// the later Session migration. They are deliberately not public wire DTOs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionBinding {
    pub schema_version: u8,
    pub id: SessionBindingId,
    pub delivery_id: DeliveryId,
    pub stage_run_id: StageRunId,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub dsh_session_id: Option<String>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub codex_session_id: Option<String>,
    pub bound_at_millis: u64,
}

pub(crate) fn validate(
    binding: &SessionBinding,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    schema_version(binding.schema_version, &format!("{path}.schemaVersion"))?;
    portable_identifier(&binding.id.0, &format!("{path}.id"))?;
    portable_identifier(&binding.delivery_id.0, &format!("{path}.deliveryId"))?;
    portable_identifier(&binding.stage_run_id.0, &format!("{path}.stageRunId"))?;
    if let Some(session_id) = &binding.dsh_session_id {
        portable_identifier(session_id, &format!("{path}.dshSessionId"))?;
    }
    if let Some(session_id) = &binding.codex_session_id {
        portable_identifier(session_id, &format!("{path}.codexSessionId"))?;
    }
    if binding.dsh_session_id.is_none() && binding.codex_session_id.is_none() {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            path,
            "session binding must reference a DSH session, a Codex session, or both",
        ));
    }
    safe_non_negative(binding.bound_at_millis, &format!("{path}.boundAtMillis"))
}

#[cfg(test)]
mod tests {
    use winwincode_domain::DeliveryId;

    use crate::domain::{Delivery, test_fixture};

    #[test]
    fn session_binding_requires_at_least_one_session_identity() {
        let mut fixture = test_fixture();
        fixture.session_bindings[0].dsh_session_id = None;
        fixture.session_bindings[0].codex_session_id = None;
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn session_binding_matches_delivery_stage_run_and_actor() {
        let mut fixture = test_fixture();
        fixture.session_bindings[0].delivery_id = DeliveryId("foreign".into());
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }
}
