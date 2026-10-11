// SPDX-License-Identifier: Apache-2.0

use serde_json::json;
use winwincode_execution_port::action_normalizer::{
    ActionRisk, FileAnalysis, FileOperation, FileRequest, McpRequest, ObservedFact,
    ToolExecutionKind, ToolRequest, observe_action,
};
use winwincode_execution_port::capability_adapter::{
    CapabilityDescriptor, CapabilityHealth, CapabilityOrigin,
};
use winwincode_execution_port::mcp_resource::{McpResourceOperation, McpResourceRequest};
use winwincode_execution_port::process_input::ProcessInputRequest;

#[test]
fn resource_methods_and_identically_named_tools_retain_distinct_authorities() {
    for operation in McpResourceOperation::ALL {
        let resource = ToolRequest::McpResource(McpResourceRequest {
            server: "fixture".into(),
            operation,
            arguments: json!({"uri":"private://TOKEN_VALUE"}),
        });
        let tool = ToolRequest::Mcp(McpRequest {
            server: "fixture".into(),
            tool: operation.method().into(),
            arguments: json!({}),
        });
        let descriptor = CapabilityDescriptor::mcp_resource(
            "fixture",
            operation,
            "1",
            CapabilityHealth::Healthy,
            CapabilityOrigin::CodexCoreMcp,
        )
        .unwrap();
        let observed = observe_action(&resource).unwrap();
        assert_eq!(observed.targets, vec![descriptor.id().to_string()]);
        assert_ne!(observed.targets, observe_action(&tool).unwrap().targets);
        assert_eq!(resource.execution_kind(), ToolExecutionKind::Mcp);
        assert_eq!(
            serde_json::from_str::<ToolRequest>(&serde_json::to_string(&resource).unwrap())
                .unwrap(),
            resource
        );
        assert!(
            !serde_json::to_string(&observed)
                .unwrap()
                .contains("TOKEN_VALUE")
        );
        assert!(!format!("{resource:?}").contains("TOKEN_VALUE"));
    }
}

#[test]
fn process_input_requires_its_origin_and_preserves_exact_bytes_for_authorization() {
    let request = ToolRequest::ProcessInput(ProcessInputRequest {
        process_id: 123,
        origin_call_id: "original-exec".into(),
        input: "printf TOKEN_VALUE\n".into(),
    });
    let observed = observe_action(&request).unwrap();
    assert_eq!(observed.minimum_risk, ActionRisk::High);
    assert_eq!(request.execution_kind(), ToolExecutionKind::Shell);
    assert_eq!(
        serde_json::from_str::<ToolRequest>(&serde_json::to_string(&request).unwrap()).unwrap(),
        request
    );
    assert!(
        !serde_json::to_string(&observed)
            .unwrap()
            .contains("TOKEN_VALUE")
    );
    assert!(!format!("{request:?}").contains("TOKEN_VALUE"));
    for invalid in [
        ProcessInputRequest {
            process_id: -1,
            origin_call_id: "origin".into(),
            input: "x".into(),
        },
        ProcessInputRequest {
            process_id: 123,
            origin_call_id: String::new(),
            input: "x".into(),
        },
        ProcessInputRequest {
            process_id: 123,
            origin_call_id: "origin".into(),
            input: String::new(),
        },
    ] {
        assert!(observe_action(&ToolRequest::ProcessInput(invalid)).is_err());
    }
}

#[test]
fn file_reads_keep_read_semantics_through_the_action_boundary() {
    let request = ToolRequest::File(FileRequest {
        operation: FileOperation::Read,
        paths: vec!["/workspace/image.png".into()],
        analysis: FileAnalysis::default(),
    });
    assert_eq!(request.execution_kind(), ToolExecutionKind::Read);
    let observed = observe_action(&request).unwrap();
    assert!(observed.facts.contains(&ObservedFact::FileRead));
    assert_eq!(observed.targets, vec!["/workspace/image.png"]);
    assert_eq!(
        serde_json::from_str::<ToolRequest>(&serde_json::to_string(&request).unwrap()).unwrap(),
        request
    );
}
