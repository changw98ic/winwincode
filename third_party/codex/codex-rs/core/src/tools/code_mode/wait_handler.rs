use serde::Deserialize;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolOutput;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use super::DEFAULT_WAIT_YIELD_TIME_MS;
use super::ExecContext;
use super::WAIT_TOOL_NAME;
use super::handle_runtime_response;
use super::telemetry::CodeModeToolCallGuard;
use super::wait_spec::create_wait_tool;

pub struct CodeModeWaitHandler;

#[derive(Debug, Deserialize)]
struct ExecWaitArgs {
    cell_id: String,
    #[serde(default = "default_wait_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    terminate: bool,
}

fn default_wait_yield_time_ms() -> u64 {
    DEFAULT_WAIT_YIELD_TIME_MS
}

fn parse_arguments<T>(arguments: &str) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
    })
}

impl ToolExecutor<ToolInvocation> for CodeModeWaitHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WAIT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_wait_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            self.handle_call(invocation)
                .await
                .map(|result| result.output)
        })
    }
}

impl CodeModeWaitHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<CoreToolOutput, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        let mut telemetry = CodeModeToolCallGuard::new(
            session.services.analytics_events_client.clone(),
            session.thread_id.to_string(),
            turn.sub_id.clone(),
            call_id.clone(),
            WAIT_TOOL_NAME,
        );
        let result = match payload {
            ToolPayload::Function { arguments }
                if tool_name.is_default_namespace()
                    && tool_name.name.as_str() == WAIT_TOOL_NAME =>
            {
                let args: ExecWaitArgs = parse_arguments(&arguments).inspect_err(|_error| {
                    telemetry.finish(/*success*/ false);
                })?;
                let exec = ExecContext { session, turn };
                let started_at = std::time::Instant::now();
                let cell_id = codex_code_mode::CellId::new(args.cell_id);
                let durable_wait =
                    match crate::tools::execution_facts::ExecutionFacts::begin_cell_wait(
                        &exec.session,
                        &exec.turn,
                        cell_id.as_str(),
                        &call_id,
                        if args.terminate {
                            0
                        } else {
                            args.yield_time_ms
                        },
                    )
                    .await
                    {
                        Ok(wait) => wait,
                        Err(FunctionCallError::RespondToModel(error)) => {
                            let mut content = vec![
                                codex_protocol::models::FunctionCallOutputContentItem::InputText {
                                    text: error,
                                },
                            ];
                            if let Some(feedback) =
                                crate::tools::tool_diagnostics::ToolDiagnostics::feedback(
                                    &exec.session,
                                    &exec.turn,
                                    &call_id,
                                )
                                .await
                            {
                                content.push(feedback);
                            }
                            telemetry.finish(/*success*/ false);
                            return Ok(CoreToolOutput {
                                output: boxed_tool_output(
                                    crate::tools::context::FunctionToolOutput::from_content(
                                        content,
                                        Some(false),
                                    ),
                                ),
                                continuation: None,
                            });
                        }
                        Err(error) => return Err(error),
                    };
                let wait_future = async {
                    if args.terminate {
                        exec.session
                            .services
                            .code_mode_service
                            .terminate(cell_id.clone())
                            .await
                    } else {
                        exec.session
                            .services
                            .code_mode_service
                            .wait(codex_code_mode::WaitRequest {
                                cell_id: cell_id.clone(),
                                yield_time_ms: args.yield_time_ms,
                            })
                            .await
                    }
                };
                let (wait_response, continuation) = exec
                    .session
                    .services
                    .code_mode_service
                    .await_boundary(&cell_id, wait_future)
                    .await
                    .map_err(|error| {
                        telemetry.finish(/*success*/ false);
                        FunctionCallError::RespondToModel(error)
                    })?;
                if let Some(durable_wait) = durable_wait {
                    durable_wait.settle().await?;
                }
                if let codex_code_mode::WaitOutcome::LiveCell(response) = &wait_response {
                    let runtime_cell_id = match response {
                        codex_code_mode::RuntimeResponse::Yielded { cell_id, .. }
                        | codex_code_mode::RuntimeResponse::Terminated { cell_id, .. }
                        | codex_code_mode::RuntimeResponse::Result { cell_id, .. } => cell_id,
                    };
                    telemetry.cell_id = Some(runtime_cell_id.to_string());
                    if let Some(executed_tool_calls) =
                        exec.session.services.executed_tool_calls.as_ref()
                    {
                        executed_tool_calls.register_cell(runtime_cell_id, &call_id);
                    }
                    if !matches!(response, codex_code_mode::RuntimeResponse::Yielded { .. }) {
                        crate::tools::execution_facts::ExecutionFacts::close_cell(
                            &exec.session,
                            runtime_cell_id.as_str(),
                        )
                        .await?;
                        exec.session
                            .services
                            .rollout_thread_trace
                            .code_cell_trace_context(
                                exec.turn.sub_id.as_str(),
                                runtime_cell_id.as_str(),
                            )
                            .record_ended(response);
                        exec.session
                            .services
                            .code_mode_service
                            .finish_cell_dispatch(runtime_cell_id);
                        exec.session
                            .services
                            .analytics_events_client
                            .track_code_mode_tool_call(
                                codex_analytics::CodeModeToolCallFact::CellClosed {
                                    thread_id: exec.session.thread_id.to_string(),
                                    turn_id: exec.turn.sub_id.clone(),
                                    cell_id: runtime_cell_id.to_string(),
                                },
                            );
                    }
                }
                exec.session.services.elicitations.wait_until_clear().await;
                let output = handle_runtime_response(
                    &exec,
                    wait_response.into(),
                    &call_id,
                    args.max_tokens,
                    started_at,
                )
                .await
                .map_err(FunctionCallError::RespondToModel)?;
                Ok(CoreToolOutput {
                    output: boxed_tool_output(output),
                    continuation,
                })
            }
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{WAIT_TOOL_NAME} expects JSON arguments"
            ))),
        };
        telemetry.finish(
            result
                .as_ref()
                .is_ok_and(|result| result.output.success_for_logging()),
        );
        result
    }
}

impl CoreToolRuntime for CodeModeWaitHandler {
    fn authorization_policy(&self) -> crate::tools::authorization::AuthorizationPolicy {
        crate::tools::authorization::AuthorizationPolicy::CoreControl
    }

    fn handle_core(
        &self,
        invocation: ToolInvocation,
    ) -> futures::future::BoxFuture<'_, Result<CoreToolOutput, FunctionCallError>> {
        Box::pin(self.handle_call(invocation))
    }

    fn pre_tool_use_payload(&self, _invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        // Code-mode `wait` is runtime control for an existing code cell, not a
        // standalone user action. Tool calls made from code mode still flow
        // through normal dispatch, but hooks should not block or rewrite the
        // wait loop itself.
        None
    }

    fn post_tool_use_payload(
        &self,
        _invocation: &ToolInvocation,
        _result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        // The wait result feeds code-mode control flow, so do not let
        // PostToolUse replace it with model-facing hook feedback.
        None
    }
}
