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

#[test]
fn custom_tool_translation_explains_json_wrapper_and_preserves_grammar() {
    let instructions =
        "Accepts raw JavaScript source text, not JSON, quoted strings, or markdown code fences.";
    let grammar = "start: pragma_source | plain_source\nplain_source: /[\\s\\S]+/";
    for namespaced in [false, true] {
        let mut exec = custom_tool("exec");
        exec["description"] = instructions.into();
        exec["format"]["definition"] = grammar.into();
        let tools = if namespaced {
            vec![repository_namespace(vec![exec]), function_tool("read-file")]
        } else {
            vec![exec, function_tool("read-file")]
        };
        let prepared = prepare_anthropic_request(
            &canonical_request(tools, Vec::new()),
            "mimo-v2.6-pro",
            options(),
        )
        .expect("prepare custom tool translation");
        let body: Value = serde_json::from_slice(&prepared.body).expect("Anthropic request body");
        let translated = &body["tools"][0];
        assert_eq!(
            translated["input_schema"],
            json!({
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"],
                "additionalProperties": false,
            })
        );
        let description = translated["description"].as_str().unwrap();
        assert!(
            description.contains(
                "Call this tool with a JSON object containing exactly one required field, \"input\", whose value is a string."
            ),
            "custom tool description must explain its required transport wrapper"
        );
        assert!(description.contains(
            "The raw-input instructions below apply to the string contents, not to the outer JSON object."
        ));
        assert!(description.contains("Type: grammar"));
        assert!(description.contains("Syntax: lark"));
        assert!(description.contains(grammar));
        assert!(description.contains(instructions));
        if namespaced {
            assert!(description.contains("Repository operations"));
        }
        let exposed_name = translated["name"].as_str().unwrap();
        assert_eq!(
            exposed_name,
            if namespaced {
                "repository-tools__exec"
            } else {
                "exec"
            }
        );
        let identity = prepared.tool_bindings.identity(exposed_name).unwrap();
        assert_eq!(identity.kind(), ProviderToolKind::Custom);
        assert_eq!(identity.name(), "exec");
        assert_eq!(
            identity.namespace(),
            namespaced.then_some("repository-tools")
        );
        assert_eq!(body["tools"][1]["description"], "Call read-file");
        assert_eq!(
            body["tools"][1]["input_schema"],
            function_tool("read-file")["parameters"]
        );
    }
}

#[test]
fn captured_mimo_empty_exec_object_reaches_core_as_function_feedback() {
    let prepared = prepare_anthropic_request(
        &canonical_request(vec![custom_tool("exec")], Vec::new()),
        "mimo-v2.6-pro",
        options(),
    )
    .expect("prepare advertised custom exec binding");
    // Minimized from the 2026-10-08 real response. Opaque IDs are replaced and
    // unrelated blocks/metadata omitted. The tool block had no argument delta.
    let response = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"mimo-response\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"mimo-v2.6-pro\",\"usage\":{\"input_tokens\":13568,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"mimo-call\",\"name\":\"exec\",\"input\":{}}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":13568,\"output_tokens\":411}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let parsed = parse_anthropic_sse(
        response.as_bytes(),
        64 * 1_024,
        64,
        &prepared.tool_bindings,
        options(),
    )
    .expect("valid empty object is retained for Core payload-kind rejection");
    assert!(parsed.events.iter().any(
        |event| matches!(event, ProviderStreamEvent::ToolCallStarted {
        index: 2, provider_call_id, identity
    } if provider_call_id == "mimo-call" && identity.kind() == ProviderToolKind::Function
        && identity.name() == "exec" && identity.namespace().is_none())
    ));
    let arguments: String = parsed
        .events
        .iter()
        .filter_map(|event| match event {
            ProviderStreamEvent::ToolCallArgumentsDelta {
                provider_call_id,
                delta,
                ..
            } if provider_call_id == "mimo-call" => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        serde_json::from_str::<Value>(&arguments).unwrap(),
        json!({})
    );
    assert!(parsed.events.iter().any(|event| matches!(event,
        ProviderStreamEvent::ToolCallEnded {index: 2, provider_call_id} if provider_call_id == "mimo-call")));
    assert!(
        matches!(parsed.terminal, ProviderGatewayTerminal::Completed {usage, ..}
        if usage.input_tokens == 13568 && usage.output_tokens == 411)
    );
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
            actual_cost_micros: Some(14),
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
    let parsed = parse_anthropic_sse(
        unknown_wire_name.as_bytes(),
        64 * 1_024,
        64,
        &prepared.tool_bindings,
        options(),
    )
    .expect("unadvertised calls reach Core error feedback");
    assert!(parsed.events.iter().any(|event| matches!(event,
        ProviderStreamEvent::ToolCallStarted { identity, .. }
        if identity.name() == "repository-tools__unbound"
            && identity.namespace() == Some("winwincode_unadvertised"))));
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
