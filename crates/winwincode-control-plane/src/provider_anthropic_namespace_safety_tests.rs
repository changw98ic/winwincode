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
