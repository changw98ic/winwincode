// SPDX-License-Identifier: Apache-2.0

//! Input to a live process retains its original invocation and exact bytes.

use serde::{Deserialize, Serialize};

use crate::action_normalizer::{
    ActionNormalizationError, ActionNormalizationErrorCode, ActionObject, ActionOperation,
    ActionRisk, ActionScope, ActionSource, ObservedAction, ObservedFact,
};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessInputRequest {
    pub process_id: i32,
    pub origin_call_id: String,
    pub input: String,
}

impl std::fmt::Debug for ProcessInputRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessInputRequest")
            .field("process_id", &self.process_id)
            .field("origin_call_id", &self.origin_call_id)
            .field("input", &"<private>")
            .finish()
    }
}

pub(crate) fn observe(
    request: &ProcessInputRequest,
) -> Result<ObservedAction, ActionNormalizationError> {
    if request.process_id <= 0
        || request.origin_call_id.trim().is_empty()
        || request.input.is_empty()
    {
        return Err(ActionNormalizationError {
            code: ActionNormalizationErrorCode::Empty,
            field: "request.processInput".into(),
            message: "requires a live process, its origin, and nonempty input".into(),
        });
    }
    // Terminal input can invoke arbitrary commands. Its bytes stay in the
    // signed request; normalized targets must not expose terminal secrets.
    Ok(ObservedAction {
        source: ActionSource::Shell,
        objects: vec![ActionObject::ExternalResource],
        operation: ActionOperation::Execute,
        scope: ActionScope::External,
        targets: vec![format!("process:{}", request.process_id)],
        minimum_risk: ActionRisk::High,
        facts: vec![ObservedFact::ShellCommand],
    })
}
