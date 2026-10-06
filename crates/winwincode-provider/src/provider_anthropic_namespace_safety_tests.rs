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

#[test]
fn malformed_stream_diagnostics_retain_shape_without_provider_text() {
    let bytes = br#"event: message_start
data: {"type":"message_start","message":{"id":"msg-private","type":"message","role":"assistant","usage":{"input_tokens":10,"output_tokens":0,"secret-marker":"never-retain-me"}}}

"#;
    let error = parse_anthropic_sse(
        bytes,
        4096,
        10,
        &AnthropicToolBindings::default(),
        options(),
    )
    .err()
    .expect("invalid fixture stream");
    let diagnostic = error.diagnostic().expect("bounded parsing context");
    assert!(diagnostic.contains("sse_event=message_start"));
    assert!(diagnostic.contains("index=0"));
    assert!(!format!("{error:?}").contains("secret-marker"));
    assert!(!format!("{error:?}").contains("never-retain-me"));
    assert!(!format!("{error:?}").contains("msg-private"));
    assert!(diagnostic.len() < 400);
}

#[test]
fn cumulative_message_deltas_and_cache_breakdown_preserve_measured_usage() {
    for cache_breakdown in [false, true] {
        let cache = if cache_breakdown {
            r#", "cache_creation":{"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":2}"#
        } else {
            ""
        };
        let wire = format!(
            r#"event: message_start
data: {{"type":"message_start","message":{{"id":"msg-deltas","type":"message","role":"assistant","usage":{{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":5{cache}}}}}}}

event: message_delta
data: {{"type":"message_delta","delta":{{"stop_reason":null}},"usage":{{"output_tokens":2}}}}

event: message_delta
data: {{"type":"message_delta","delta":{{"stop_reason":"end_turn"}},"usage":{{"output_tokens":3}}}}

event: message_stop
data: {{"type":"message_stop"}}

"#
        );
        let parsed = parse_anthropic_sse(
            wire.as_bytes(),
            4096,
            10,
            &AnthropicToolBindings::default(),
            options(),
        )
        .expect("cumulative usage updates before the final stop reason");
        assert!(
            matches!(parsed.terminal, ProviderGatewayTerminal::Completed { usage, .. } if usage.input_tokens == 15 && usage.cache_write_input_tokens == 5 && usage.output_tokens == 3)
        );
        for invalid in [
            wire.replace(
                "\"ephemeral_1h_input_tokens\":2",
                "\"ephemeral_1h_input_tokens\":3",
            ),
            wire.replace("\"output_tokens\":3", "\"output_tokens\":1"),
            wire.replace("\"stop_reason\":\"end_turn\"", "\"stop_reason\":null"),
        ] {
            if invalid != wire {
                assert!(
                    parse_anthropic_sse(
                        invalid.as_bytes(),
                        4096,
                        10,
                        &AnthropicToolBindings::default(),
                        options()
                    )
                    .is_err(),
                    "inconsistent accounting or missing stop reason must fail closed"
                );
            }
        }
    }
}

fn function_tool(name: &str) -> Value {
    json!({
        "type": "function",
        "name": name,
        "description": "Function fixture",
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
        "description": "Custom fixture",
        "format": {
            "type": "grammar",
            "syntax": "lark",
            "definition": "start: /.+/"
        }
    })
}

fn namespace(name: &str, tool: Value) -> Value {
    json!({"type": "namespace", "name": name, "tools": Value::Array(vec![tool])})
}

fn canonical_request(tools: Vec<Value>, history: Vec<Value>) -> Vec<u8> {
    let mut input = vec![json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": "Use both tools."}]
    })];
    input.extend(history);
    serde_json::to_vec(&json!({
        "requestId": "codex-request-namespace-safety",
        "provider": "winwincode",
        "sessionId": "session-namespace-safety",
        "threadId": "thread-namespace-safety",
        "turnId": "turn-namespace-safety",
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
    .expect("canonical namespace safety request")
}

fn colliding_tools() -> Vec<Value> {
    vec![
        namespace("a", function_tool("b__c")),
        namespace("a__b", custom_tool("c")),
    ]
}

fn colliding_alias_sse() -> String {
    [
        r#"event: message_start
data: {"type":"message_start","message":{"id":"msg-alias-collision","type":"message","role":"assistant","usage":{"input_tokens":2,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":4,"content_block":{"type":"tool_use","id":"call-function","name":"a__b__c","input":{}}}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call-custom","name":"a__b__c_2","input":{}}}

"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"patch\"}"}}

event: content_block_delta
data: {"type":"content_block_delta","index":4,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"README.md\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":4}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":4}}

event: message_stop
data: {"type":"message_stop"}

"#,
    ]
    .concat()
}

#[test]
fn colliding_cross_namespace_aliases_round_trip_through_explicit_bindings() {
    let prepared = prepare_anthropic_request(
        &canonical_request(colliding_tools(), Vec::new()),
        "glm-5.2",
        options(),
    )
    .expect("prepare colliding cross-namespace aliases");
    let body: Value = serde_json::from_slice(&prepared.body).expect("Anthropic request body");
    assert_eq!(body["tools"][0]["name"], "a__b__c");
    assert_eq!(body["tools"][1]["name"], "a__b__c_2");

    let parsed = parse_anthropic_sse(
        colliding_alias_sse().as_bytes(),
        64 * 1_024,
        64,
        &prepared.tool_bindings,
        options(),
    )
    .expect("parse colliding aliases through frozen bindings");
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
    assert_eq!(identities[0].name(), "b__c");
    assert_eq!(identities[0].namespace(), Some("a"));
    assert_eq!(identities[1].kind(), ProviderToolKind::Custom);
    assert_eq!(identities[1].name(), "c");
    assert_eq!(identities[1].namespace(), Some("a__b"));
}

fn request_error(history: Vec<Value>) -> AnthropicCodecError {
    let Err(error) = prepare_anthropic_request(
        &canonical_request(colliding_tools(), history),
        "glm-5.2",
        options(),
    ) else {
        panic!("history with a changed tool kind was accepted");
    };
    error
}

#[test]
fn history_kind_cannot_rebind_an_exact_name_and_namespace() {
    let wrong_custom = request_error(vec![json!({
        "type": "custom_tool_call",
        "name": "b__c",
        "namespace": "a",
        "input": "safety-marker-custom",
        "call_id": "call-wrong-custom"
    })]);
    assert_eq!(wrong_custom.kind(), AnthropicCodecErrorKind::InvalidRequest);
    assert!(!format!("{wrong_custom:?}").contains("safety-marker-custom"));

    let wrong_function = request_error(vec![json!({
        "type": "function_call",
        "name": "c",
        "namespace": "a__b",
        "arguments": "{\"path\":\"safety-marker-function\"}",
        "call_id": "call-wrong-function"
    })]);
    assert_eq!(
        wrong_function.kind(),
        AnthropicCodecErrorKind::InvalidRequest
    );
    assert!(!format!("{wrong_function:?}").contains("safety-marker-function"));
}

#[test]
fn compaction_translates_history_without_advertising_or_authorizing_tools() {
    let history = vec![
        json!({"type":"function_call","name":"exec_command","arguments":"{}","call_id":"call-exec"}),
        json!({"type":"function_call_output","call_id":"call-exec","output":"completed"}),
        json!({"type":"custom_tool_call","name":"apply_patch","namespace":"functions","input":"patch","call_id":"call-patch"}),
        json!({"type":"custom_tool_call_output","call_id":"call-patch","output":"completed"}),
    ];
    let input = canonical_request(Vec::new(), history);
    let prepared = prepare_anthropic_request(&input, "deepseek-flash", options())
        .expect("compaction history remains translatable with no advertised tools");
    let body: Value = serde_json::from_slice(&prepared.body).unwrap();
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
    assert_eq!(body["messages"][1]["content"][0]["name"], "exec_command");
    assert_eq!(
        body["messages"][3]["content"][0]["name"],
        "functions__apply_patch"
    );
    for name in ["exec_command", "functions__apply_patch"] {
        let identity = prepared.tool_bindings.response_identity(name).unwrap();
        assert_eq!(identity.namespace(), Some(UNADVERTISED_TOOL_NAMESPACE));
    }
    let chat =
        crate::provider_openai::prepare_openai_chat_request(&input, "deepseek-flash", options())
            .expect("OpenAI compaction uses the same historical identity translation");
    let body: Value = serde_json::from_slice(&chat.body).unwrap();
    assert!(body.get("tools").is_none());
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["function"]["name"],
        "exec_command"
    );
}
