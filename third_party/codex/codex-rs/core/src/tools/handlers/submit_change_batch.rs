use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

pub(crate) const SUBMIT_CHANGE_BATCH_TOOL_NAME: &str = "submit_change_batch";
const SUBMIT_CHANGE_BATCH_GRAMMAR: &str = include_str!("submit_change_batch.lark");

/// The terminal delegated tool is advertised only on turns explicitly opted
/// into host handoff. Its runtime is intercepted by Core before dispatch; a
/// direct handler is deliberately fail-closed so it cannot mutate a workspace.
pub(crate) struct SubmitChangeBatchHandler;

impl ToolExecutor<ToolInvocation> for SubmitChangeBatchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(SUBMIT_CHANGE_BATCH_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Freeform(FreeformTool {
            name: SUBMIT_CHANGE_BATCH_TOOL_NAME.to_string(),
            description: "Submit one bounded ChangeBatch proposal to the host. This terminal tool only hands off the proposal; it does not modify files or run validation.".to_string(),
            defer_loading: None,
            format: FreeformToolFormat {
                r#type: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: SUBMIT_CHANGE_BATCH_GRAMMAR.to_string(),
            },
        })
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async {
            Err(FunctionCallError::Fatal(
                "submit_change_batch must be handled by the host handoff boundary".to_string(),
            ))
        })
    }
}

impl CoreToolRuntime for SubmitChangeBatchHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Custom { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::SUBMIT_CHANGE_BATCH_GRAMMAR;

    #[test]
    fn grammar_uses_the_canonical_closed_field_order() {
        assert!(
            SUBMIT_CHANGE_BATCH_GRAMMAR
                .starts_with("start: \"{\" ws \"\\\"acceptanceCriteriaIds\\\"\"")
        );
        for field in [
            "acceptanceCriteriaIds",
            "disposition",
            "patch",
            "schemaVersion",
            "validationProfile",
        ] {
            assert!(SUBMIT_CHANGE_BATCH_GRAMMAR.contains(field));
        }
        assert!(SUBMIT_CHANGE_BATCH_GRAMMAR.contains("\\\"final\\\""));
        assert!(SUBMIT_CHANGE_BATCH_GRAMMAR.contains("\\\"continue\\\""));
        assert!(SUBMIT_CHANGE_BATCH_GRAMMAR.contains("\\\"probe\\\""));
    }
}
