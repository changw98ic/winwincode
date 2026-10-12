// SPDX-License-Identifier: Apache-2.0
use super::ContextualUserFragment;

/// A bounded recovery observation for an exec call whose result was not retained.
pub(crate) struct CodeModeResultUnavailable;
impl ContextualUserFragment for CodeModeResultUnavailable {
    fn role(&self) -> &'static str {
        "developer"
    }
    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }
    fn type_markers() -> (&'static str, &'static str) {
        (
            "<code_mode_result_unavailable>",
            "</code_mode_result_unavailable>",
        )
    }
    fn body(&self) -> String {
        serde_json::json!({
            "type": "code_mode_result_unavailable", "schema_version": 1,
            "result": "unavailable", "execution": "outcome_unknown",
            "next_step": "Read durable Core tool facts before retrying side effects. Resume pending user input through its original request identity.",
        }).to_string()
    }
}
