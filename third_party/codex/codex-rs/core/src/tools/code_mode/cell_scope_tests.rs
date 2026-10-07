// SPDX-License-Identifier: Apache-2.0

use super::validate;
use crate::session::step_context::StepContext;
use crate::tools::context::ToolInvocation;
use crate::tools::handlers::ToolSearchHandlerCache;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolRouter;
use codex_features::Feature;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use pretty_assertions::assert_eq;
use std::sync::Arc;

struct CatalogTool {
    spec: ToolSpec,
    server: Option<String>,
}

impl ToolExecutor<ToolInvocation> for CatalogTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("example")
    }
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        panic!("scope validation must precede execution");
    }
}
impl CoreToolRuntime for CatalogTool {
    fn mcp_server_name(&self) -> Option<&str> {
        self.server.as_deref()
    }
}

#[tokio::test]
async fn cell_scope_checks_definition_eligibility_and_connection_identity() {
    let (_, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config)
        .features
        .enable(Feature::CodeModeOnly)
        .unwrap();
    let turn = Arc::new(turn);
    let spec = ToolSpec::Function(codex_tools::ResponsesApiTool {
        name: "example".to_string(),
        description: "Original schema".to_string(),
        strict: false,
        defer_loading: None,
        parameters: serde_json::from_value(
            serde_json::json!({"type":"object","properties":{"value":{"type":"string"}}}),
        )
        .unwrap(),
        output_schema: None,
    });
    let router = |spec: ToolSpec, server: Option<String>| {
        Arc::new(ToolRouter::from_registry(
            &turn,
            ToolRegistry::from_tools([
                Arc::new(CatalogTool { spec, server }) as Arc<dyn CoreToolRuntime>
            ]),
            Vec::new(),
            &ToolSearchHandlerCache::default(),
        ))
    };
    let name = ToolName::plain("example");
    let mut origin = StepContext::for_test(Arc::clone(&turn));
    Arc::get_mut(&mut origin).unwrap().tool_router =
        router(spec.clone(), Some("fixture".to_string()));
    let mut current = StepContext::for_test(Arc::clone(&turn));
    Arc::get_mut(&mut current).unwrap().tool_router =
        router(spec.clone(), Some("fixture".to_string()));
    Arc::get_mut(&mut current).unwrap().mcp = Arc::clone(&origin.mcp);
    assert_eq!(validate(&origin, &current, &name), Ok(()));

    let mut changed_spec = spec.clone();
    if let ToolSpec::Function(tool) = &mut changed_spec {
        tool.parameters = serde_json::from_value(
            serde_json::json!({"type":"object","properties":{"value":{"type":"integer"}}}),
        )
        .unwrap();
    }
    Arc::get_mut(&mut current).unwrap().tool_router =
        router(changed_spec, Some("fixture".to_string()));
    assert!(
        validate(&origin, &current, &name)
            .unwrap_err()
            .contains("changed after this cell started")
    );

    Arc::get_mut(&mut current).unwrap().tool_router = Arc::new(ToolRouter::from_registry(
        &turn,
        ToolRegistry::empty_for_test(),
        Vec::new(),
        &ToolSearchHandlerCache::default(),
    ));
    assert!(
        validate(&origin, &current, &name)
            .unwrap_err()
            .contains("no longer authorized")
    );

    Arc::get_mut(&mut current).unwrap().tool_router =
        router(spec.clone(), Some("fixture".to_string()));
    Arc::get_mut(&mut current).unwrap().mcp =
        Arc::clone(&StepContext::for_test(Arc::clone(&turn)).mcp);
    assert!(
        validate(&origin, &current, &name)
            .unwrap_err()
            .contains("different MCP catalog or account session")
    );

    Arc::get_mut(&mut origin).unwrap().tool_router = router(spec.clone(), None);
    Arc::get_mut(&mut current).unwrap().tool_router = router(spec, None);
    assert_eq!(validate(&origin, &current, &name), Ok(()));
}

#[tokio::test]
async fn mcp_resources_keep_the_same_connection_scope_as_mcp_tool_calls() {
    let (_, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config)
        .features
        .enable(Feature::CodeModeOnly)
        .unwrap();
    let turn = Arc::new(turn);
    let router = Arc::new(ToolRouter::from_registry(
        &turn,
        ToolRegistry::from_tools([
            Arc::new(crate::tools::handlers::ReadMcpResourceHandler) as Arc<dyn CoreToolRuntime>
        ]),
        Vec::new(),
        &ToolSearchHandlerCache::default(),
    ));
    let mut origin = StepContext::for_test(Arc::clone(&turn));
    let mut current = StepContext::for_test(turn);
    Arc::get_mut(&mut origin).unwrap().tool_router = Arc::clone(&router);
    Arc::get_mut(&mut current).unwrap().tool_router = router;
    let name = ToolName::plain("read_mcp_resource");
    assert!(
        validate(&origin, &current, &name)
            .unwrap_err()
            .contains("different MCP catalog or account session")
    );
    Arc::get_mut(&mut current).unwrap().mcp = Arc::clone(&origin.mcp);
    assert_eq!(validate(&origin, &current, &name), Ok(()));
}
