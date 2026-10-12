// SPDX-License-Identifier: Apache-2.0

use crate::session::step_context::StepContext;
use codex_tools::ToolName;
use std::sync::Arc;

/// Verify a live cell's captured definition and connection scope before dispatch.
/// The current step still owns execution permissions, hooks, and authorization.
pub(super) fn validate(
    origin: &StepContext,
    current: &StepContext,
    name: &ToolName,
) -> Result<(), String> {
    let previous = origin.tool_router.code_mode_tool_runtime(name);
    let latest = current.tool_router.code_mode_tool_runtime(name);
    let (Some(previous), Some(latest)) = (previous, latest) else {
        return Err(format!(
            "Code Mode tool `{name}` is no longer authorized in this catalog; discover tools in a new exec cell"
        ));
    };
    if previous.spec() != latest.spec() {
        return Err(format!(
            "Code Mode tool `{name}` changed after this cell started; load its definition in a new exec cell"
        ));
    }
    if (previous.uses_mcp_binding() || latest.uses_mcp_binding())
        && !Arc::ptr_eq(&origin.mcp, &current.mcp)
    {
        return Err(format!(
            "Code Mode tool `{name}` has a different MCP catalog or account session; discover tools in a new exec cell"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "cell_scope_tests.rs"]
mod tests;
