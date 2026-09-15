// SPDX-License-Identifier: Apache-2.0

use super::*;

fn options() -> AnthropicMessagesOptions {
    AnthropicMessagesOptions {
        max_output_tokens: 4_096,
        pricing: ProviderTokenPricing {
            input_micros_per_million_tokens: 1_000_000,
            output_micros_per_million_tokens: 3_000_000,
            ..ProviderTokenPricing::default()
        },
    }
}

fn function_tool(name: &str) -> Value {
    json!({
        "type": "function",
        "name": name,
        "description": format!("Call {name}"),
        "strict": false,
        "parameters": {
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }
    })
}

fn custom_tool(name: &str) -> Value {
    json!({
        "type": "custom",
        "name": name,
        "description": format!("Call {name}"),
        "format": {
            "type": "grammar",
            "syntax": "lark",
            "definition": "start: /.+/"
        }
    })
}

fn canonical_request(tools: Vec<Value>, history: Vec<Value>) -> Vec<u8> {
    let mut input = vec![json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": "Inspect the repository."}]
    })];
    input.extend(history);
    serde_json::to_vec(&json!({
        "requestId": "codex-request-namespace-1",
        "provider": "winwincode",
        "sessionId": "session-namespace-1",
        "threadId": "thread-namespace-1",
        "turnId": "turn-namespace-1",
        "request": {
            "model": "local-model[1m]",
            "instructions": "Use the declared tools.",
            "input": input,
            "tools": Value::Array(tools),
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "reasoning": null,
            "store": false,
            "stream": true,
            "stream_options": null,
            "include": [],
            "service_tier": null,
            "prompt_cache_key": null,
            "text": null,
            "client_metadata": null
        }
    }))
    .expect("canonical namespaced request JSON")
}

fn repository_namespace(children: Vec<Value>) -> Value {
    json!({
        "type": "namespace",
        "name": "repository-tools",
        "description": "Repository operations",
        "tools": Value::Array(children)
    })
}

fn prepared_namespaced_request() -> PreparedAnthropicRequest {
    let tools = vec![repository_namespace(vec![
        function_tool("read-file"),
        custom_tool("apply-patch"),
    ])];
    let history = vec![
        json!({
            "type": "reasoning",
            "id": "reasoning-namespace-1",
            "summary": [{"type": "summary_text", "text": "Run both tools."}],
            "content": [],
            "encrypted_content": null
        }),
        json!({
            "type": "function_call",
            "name": "read-file",
            "namespace": "repository-tools",
            "arguments": "{\"path\":\"src/lib.rs\"}",
            "call_id": "call-function"
        }),
        json!({
            "type": "custom_tool_call",
            "name": "apply-patch",
            "namespace": "repository-tools",
            "input": "*** Begin Patch",
            "call_id": "call-custom"
        }),
    ];
    prepare_anthropic_request(&canonical_request(tools, history), "glm-5.2", options())
        .expect("prepare namespaced Anthropic request")
}

#[test]
fn namespaced_request_and_history_use_bound_aliases_without_losing_identity() {
    let prepared = prepared_namespaced_request();
    let body: Value = serde_json::from_slice(&prepared.body).expect("Anthropic request body");
    assert_eq!(body["tools"][0]["name"], "repository-tools__read-file");
    assert_eq!(body["tools"][1]["name"], "repository-tools__apply-patch");
    assert_eq!(
        body["tools"][0]["description"],
        "Repository operations\n\nCall read-file"
    );
    let assistant = &body["messages"][1]["content"];
    assert_eq!(assistant[0]["text"], "Run both tools.");
    assert_eq!(assistant[1]["name"], "repository-tools__read-file");
    assert_eq!(assistant[2]["name"], "repository-tools__apply-patch");

    let read_identity = prepared
        .tool_bindings
        .identity("repository-tools__read-file")
        .expect("bound read identity");
    assert_eq!(read_identity.kind(), ProviderToolKind::Function);
    assert_eq!(read_identity.name(), "read-file");
    assert_eq!(read_identity.namespace(), Some("repository-tools"));
    let patch_identity = prepared
        .tool_bindings
        .identity("repository-tools__apply-patch")
        .expect("bound patch identity");
    assert_eq!(patch_identity.kind(), ProviderToolKind::Custom);
    assert_eq!(patch_identity.name(), "apply-patch");
    assert_eq!(patch_identity.namespace(), Some("repository-tools"));
}

fn parallel_tool_sse() -> String {
    [
        r#"event: message_start
data: {"type":"message_start","message":{"id":"msg-namespace","type":"message","role":"assistant","usage":{"input_tokens":2,"output_tokens":0}}}

"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":7,"content_block":{"type":"tool_use","id":"call-function","name":"repository-tools__read-file","input":{}}}

event: content_block_start
data: {"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"call-custom","name":"repository-tools__apply-patch","input":{}}}

"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"patch\"}"}}

event: content_block_delta
data: {"type":"content_block_delta","index":7,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src/lib.rs\"}"}}

"#,
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":7}

event: content_block_stop
data: {"type":"content_block_stop","index":3}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":4}}

event: message_stop
data: {"type":"message_stop"}

"#,
    ]
    .concat()
}

#[test]
fn parallel_namespaced_sse_round_trips_the_bound_canonical_identities() {
    let prepared = prepared_namespaced_request();
    let parsed = parse_anthropic_sse(
        parallel_tool_sse().as_bytes(),
        64 * 1_024,
        64,
        &prepared.tool_bindings,
        options(),
    )
    .expect("parse namespaced parallel Anthropic SSE");
    let identities = parsed
        .events
        .iter()
        .filter_map(|event| match event {
            ProviderStreamEvent::ToolCallStarted { identity, .. } => Some(identity),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(identities.len(), 2);
    assert_eq!(identities[0].kind(), ProviderToolKind::Function);
    assert_eq!(identities[0].name(), "read-file");
    assert_eq!(identities[0].namespace(), Some("repository-tools"));
    assert_eq!(identities[1].kind(), ProviderToolKind::Custom);
    assert_eq!(identities[1].name(), "apply-patch");
    assert_eq!(identities[1].namespace(), Some("repository-tools"));
    assert!(matches!(
        parsed.terminal,
        ProviderGatewayTerminal::Completed {
            usage: ProviderTokenUsage {
                input_tokens: 2,
                output_tokens: 4,
                ..
            },
            actual_cost_micros: 14,
        }
    ));
}

fn request_error(tools: Vec<Value>, history: Vec<Value>) -> AnthropicCodecErrorKind {
    let Err(error) =
        prepare_anthropic_request(&canonical_request(tools, history), "glm-5.2", options())
    else {
        panic!("invalid namespaced tool request was accepted");
    };
    error.kind()
}

#[test]
fn invalid_conflicting_and_oversized_namespaces_fail_closed() {
    let invalid_namespace = vec![json!({
        "type": "namespace",
        "name": "repository.tools",
        "tools": [function_tool("read-file")]
    })];
    assert_eq!(
        request_error(invalid_namespace, Vec::new()),
        AnthropicCodecErrorKind::InvalidRequest
    );

    let oversized_namespace = vec![json!({
        "type": "namespace",
        "name": "n".repeat(65),
        "tools": [function_tool("read-file")]
    })];
    assert_eq!(
        request_error(oversized_namespace, Vec::new()),
        AnthropicCodecErrorKind::InvalidRequest
    );

    let oversized_name = vec![repository_namespace(vec![function_tool(&"t".repeat(129))])];
    assert_eq!(
        request_error(oversized_name, Vec::new()),
        AnthropicCodecErrorKind::InvalidRequest
    );

    let duplicate = vec![repository_namespace(vec![
        function_tool("read-file"),
        custom_tool("read-file"),
    ])];
    assert_eq!(
        request_error(duplicate, Vec::new()),
        AnthropicCodecErrorKind::InvalidRequest
    );
}

#[test]
fn alias_collisions_are_deterministic_and_never_parsed_as_identity() {
    let tools = vec![
        function_tool("repository-tools__read-file"),
        repository_namespace(vec![function_tool("read-file")]),
    ];
    let prepared =
        prepare_anthropic_request(&canonical_request(tools, Vec::new()), "glm-5.2", options())
            .expect("prepare colliding aliases");
    let body: Value = serde_json::from_slice(&prepared.body).expect("Anthropic request body");
    assert_eq!(body["tools"][0]["name"], "repository-tools__read-file");
    assert_eq!(body["tools"][1]["name"], "repository-tools__read-file_2");
    assert_eq!(
        prepared
            .tool_bindings
            .identity("repository-tools__read-file")
            .expect("root binding")
            .namespace(),
        None
    );
    assert_eq!(
        prepared
            .tool_bindings
            .identity("repository-tools__read-file_2")
            .expect("namespaced binding")
            .namespace(),
        Some("repository-tools")
    );

    let unknown_wire_name =
        parallel_tool_sse().replace("repository-tools__read-file", "repository-tools__unbound");
    let Err(error) = parse_anthropic_sse(
        unknown_wire_name.as_bytes(),
        64 * 1_024,
        64,
        &prepared.tool_bindings,
        options(),
    ) else {
        panic!("unbound Anthropic alias was parsed as a canonical identity");
    };
    assert_eq!(error.kind(), AnthropicCodecErrorKind::Protocol);
}

#[test]
fn namespaced_history_must_match_the_exact_declared_binding() {
    let tools = vec![repository_namespace(vec![function_tool("read-file")])];
    let wrong_namespace = vec![json!({
        "type": "function_call",
        "name": "read-file",
        "namespace": "other-tools",
        "arguments": "{\"path\":\"src/lib.rs\"}",
        "call_id": "call-wrong-namespace"
    })];
    assert_eq!(
        request_error(tools.clone(), wrong_namespace),
        AnthropicCodecErrorKind::InvalidRequest
    );

    let missing_namespace = vec![json!({
        "type": "function_call",
        "name": "read-file",
        "arguments": "{\"path\":\"src/lib.rs\"}",
        "call_id": "call-missing-namespace"
    })];
    assert_eq!(
        request_error(tools, missing_namespace),
        AnthropicCodecErrorKind::InvalidRequest
    );
}
