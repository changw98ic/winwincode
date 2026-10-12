use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::parallel::ChangeBatchHandoff;
use crate::tools::parallel::ToolContinuation;
use crate::tools::registry::CoreToolOutput;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

pub(crate) const SUBMIT_CHANGE_BATCH_TOOL_NAME: &str = "submit_change_batch";
const SUBMIT_CHANGE_BATCH_GRAMMAR: &str = include_str!("submit_change_batch.lark");

/// The terminal delegated tool is advertised only on turns explicitly opted
/// into host handoff. The common dispatch boundary authorizes its proposal and
/// runs hooks before accepting the terminal continuation.
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

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            self.handle_core(invocation)
                .await
                .map(|result| result.output)
        })
    }
}

impl CoreToolRuntime for SubmitChangeBatchHandler {
    fn authorization_policy(&self) -> crate::tools::authorization::AuthorizationPolicy {
        crate::tools::authorization::AuthorizationPolicy::CoreControl
    }

    fn handle_core(
        &self,
        invocation: ToolInvocation,
    ) -> futures::future::BoxFuture<'_, Result<CoreToolOutput, FunctionCallError>> {
        Box::pin(async move {
            let ToolPayload::Custom { input } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "submit_change_batch expects a ChangeBatch proposal".to_string(),
                ));
            };
            if !invocation.turn.submit_change_batch {
                return Err(FunctionCallError::RespondToModel(
                    "this turn does not allow host handoff".to_string(),
                ));
            }
            Ok(CoreToolOutput {
                output: Box::new(FunctionToolOutput::from_text(
                    "ChangeBatch proposal handed to host for application and validation."
                        .to_string(),
                    Some(true),
                )),
                continuation: Some(ToolContinuation::YieldToHost(ChangeBatchHandoff {
                    call_id: invocation.call_id,
                    proposal: input,
                })),
            })
        })
    }

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
