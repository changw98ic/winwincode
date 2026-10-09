// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::fmt::Write;

fn options() -> AnthropicMessagesOptions {
    AnthropicMessagesOptions {
        max_output_tokens: 1024,
        pricing: ProviderTokenPricing::default(),
    }
}

fn start() -> Value {
    json!({"type":"message_start", "message":{
        "id":"response-1", "type":"message", "role":"assistant", "model":"deepseek-flash",
        "usage":{"input_tokens":12, "output_tokens":0}
    }})
}

fn finish() -> Value {
    json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}, "usage":{"output_tokens":3}})
}

fn wire(events: &[Value]) -> Vec<u8> {
    let mut wire = String::new();
    for event in events {
        write!(
            wire,
            "event: {}\nx-vendor-metadata: ignored\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        )
        .expect("write test SSE");
    }
    wire.into_bytes()
}

fn parse(events: &[Value]) -> Result<ParsedAnthropicStream, AnthropicCodecError> {
    parse_anthropic_sse(
        &wire(events),
        64 * 1024,
        64,
        &AnthropicToolBindings::default(),
        options(),
    )
}

#[test]
fn optional_vendor_metadata_is_ignored_at_each_response_layer() {
    let mut events = vec![
        start(),
        json!({"type":"ping"}),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":""}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":"answer"}}),
        json!({"type":"content_block_stop", "index":0}),
        json!({"type":"content_block_start", "index":1, "content_block":{"type":"tool_use", "id":"call-1", "name":"read_file", "input":{}}}),
        json!({"type":"content_block_delta", "index":1, "delta":{"type":"input_json_delta", "partial_json":"{\"path\":\"src/lib.rs\"}"}}),
        json!({"type":"content_block_stop", "index":1}),
        finish(),
        json!({"type":"message_stop"}),
    ];
    let expected = parse(&events).expect("canonical response");
    for event in &mut events {
        event["vendor_extension"] = json!({"opaque":"uninterpreted"});
        if event.get("message").is_some() {
            event["message"]["vendor_extension"] = json!(true);
            event["message"]["usage"]["cache_creation"] = json!({"ephemeral_5m_input_tokens":0});
        }
        if event.get("content_block").is_some() {
            event["content_block"]["vendor_extension"] = json!([1, 2]);
        }
        if event.get("delta").is_some() {
            event["delta"]["vendor_extension"] = json!(false);
        }
        if event.get("usage").is_some() {
            event["usage"]["vendor_extension"] = json!({"version":2});
        }
    }
    let actual = parse(&events).expect("extended response");
    assert_eq!(actual.events, expected.events);
    assert_eq!(actual.terminal, expected.terminal);

    let extended_error = parse(&[json!({"type":"error", "request_id":"ignored", "error":{
        "type":"rate_limit_error", "message":"private diagnostic text", "vendor_extension":{}
    }})])
    .expect("extended error envelope");
    assert!(
        matches!(extended_error.events.as_slice(), [ProviderStreamEvent::Failed(failure)]
        if failure.kind() == ProviderStreamFailureKind::RateLimit)
    );
}

#[test]
fn consumed_response_fields_remain_typed_and_diagnostics_never_copy_values() {
    let cases = [
        (
            vec![json!({"type":"message_start", "message":{"id":42}})],
            "message_start",
            "$.message.id",
        ),
        (
            vec![
                json!({"type":"message_start", "message":{"id":"response-1", "type":"message", "role":"assistant", "usage":{"input_tokens":"private-secret", "output_tokens":0}}}),
            ],
            "message_start",
            "$.message.usage.input_tokens",
        ),
        (
            vec![
                start(),
                json!({"type":"content_block_start", "index":"private-secret", "content_block":{"type":"text", "text":""}}),
            ],
            "content_block_start",
            "$.index",
        ),
        (
            vec![
                start(),
                json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "id":"private-secret\n", "name":"read_file", "input":{}}}),
            ],
            "content_block_start",
            "$.content_block.id",
        ),
        (
            vec![
                start(),
                json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":""}}),
                json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":{"private-secret":true}}}),
            ],
            "content_block_delta",
            "$.delta.text",
        ),
        (
            vec![
                start(),
                json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{"output_tokens":"private-secret"}}),
            ],
            "message_delta",
            "$.usage.output_tokens",
        ),
        (
            vec![
                json!({"type":"error", "error":{"type":"rate_limit_error", "message":42, "private-secret":true}}),
            ],
            "error",
            "$.error.message",
        ),
    ];
    for (events, event_type, field_path) in cases {
        let Err(error) = parse(&events) else {
            panic!("malformed known field accepted");
        };
        assert_eq!(error.kind(), AnthropicCodecErrorKind::Protocol);
        assert_eq!(
            error.diagnostic(),
            Some(AnthropicCodecDiagnostic {
                stage: "response_fields",
                event_type,
                field_path,
            })
        );
        assert!(!format!("{error:?}").contains("private-secret"));
    }
    let Err(error) = parse(&[json!({"type":"private-secret"})]) else {
        panic!("unknown event accepted");
    };
    assert_eq!(error.diagnostic().unwrap().event_type, "unknown");
    assert!(!format!("{error:?}").contains("private-secret"));
}

#[test]
fn extensions_preserve_event_order_and_invalid_wrapper_payloads() {
    let invalid_inputs = [json!([]), json!("secret"), json!(null)];
    for input in invalid_inputs {
        let Err(error) = parse(&[
            start(),
            json!({"type":"content_block_start", "index":0, "vendor":true,
            "content_block":{"type":"tool_use", "id":"call-1", "name":"read_file", "input":input, "vendor":true}}),
        ]) else {
            panic!("non-object tool arguments accepted");
        };
        assert_eq!(error.kind(), AnthropicCodecErrorKind::Protocol);
    }
    for events in [
        vec![start(), start()],
        vec![
            start(),
            json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":"x"}, "vendor":true}),
        ],
        vec![
            start(),
            json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":""}}),
            finish(),
        ],
    ] {
        assert!(
            matches!(parse(&events), Err(error) if error.kind() == AnthropicCodecErrorKind::Protocol)
        );
    }
    let mut bindings = AnthropicToolBindings::default();
    bindings
        .insert(
            "apply_patch".to_owned(),
            ProviderToolIdentity::try_new(ProviderToolKind::Custom, "apply_patch".to_owned(), None)
                .unwrap(),
        )
        .unwrap();
    let body = wire(&[
        start(),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "id":"call-1", "name":"apply_patch", "input":{"input":"patch", "extra":true}}}),
        json!({"type":"content_block_stop", "index":0}),
        finish(),
        json!({"type":"message_stop"}),
    ]);
    let parsed = parse_anthropic_sse(&body, 64 * 1024, 64, &bindings, options())
        .expect("valid object with extra wrapper field reaches Core as a function call");
    assert!(parsed.events.iter().any(
        |event| matches!(event, ProviderStreamEvent::ToolCallStarted {
        index: 0, provider_call_id, identity
    } if provider_call_id == "call-1" && identity.kind() == ProviderToolKind::Function
        && identity.name() == "apply_patch" && identity.namespace().is_none())
    ));
    let arguments: String = parsed
        .events
        .iter()
        .filter_map(|event| match event {
            ProviderStreamEvent::ToolCallArgumentsDelta {
                provider_call_id,
                delta,
                ..
            } if provider_call_id == "call-1" => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        serde_json::from_str::<Value>(&arguments).unwrap(),
        json!({"input":"patch", "extra":true})
    );
    assert!(parsed.events.iter().any(|event| matches!(event,
        ProviderStreamEvent::ToolCallEnded {index: 0, provider_call_id} if provider_call_id == "call-1")));
    assert!(
        matches!(parsed.terminal, ProviderGatewayTerminal::Completed {usage, ..}
        if usage.input_tokens == 12 && usage.output_tokens == 3)
    );
}

#[test]
fn request_translation_still_rejects_unknown_canonical_fields() {
    let request = json!({"requestId":"r-1", "provider":"winwincode", "sessionId":"s-1", "threadId":"t-1", "request":{
        "model":"local", "instructions":"", "stream":true, "store":false, "tool_choice":"auto", "parallel_tool_calls":true,
        "tools":[], "input":[{"type":"message", "role":"user", "content":[{"type":"input_text", "text":"hello"}]}]
    }});
    prepare_anthropic_request(
        &serde_json::to_vec(&request).unwrap(),
        "deepseek-flash",
        options(),
    )
    .expect("valid canonical request");
    for location in [
        "",
        "/request",
        "/request/input/0",
        "/request/input/0/content/0",
    ] {
        let mut invalid = request.clone();
        invalid.pointer_mut(location).unwrap()["vendor_extension"] = json!(true);
        assert!(
            matches!(prepare_anthropic_request(&serde_json::to_vec(&invalid).unwrap(), "deepseek-flash", options()),
            Err(error) if error.kind() == AnthropicCodecErrorKind::InvalidRequest)
        );
    }
}

#[test]
fn malformed_tool_json_retains_observed_final_usage_without_stream_success() {
    let events = [
        start(),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "id":"call-1", "name":"read_file", "input":{}}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":"private-secret"}}),
        json!({"type":"content_block_stop", "index":0}),
        finish(),
    ];
    assert!(parse(&events).is_err());
    let (response_id, usage) = observed_anthropic_receipt(&wire(&events), 64 * 1024, 64, options())
        .expect("genuine final usage");
    assert_eq!(response_id, "response-1");
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.output_tokens, 3);
    let mut interrupted = wire(&events);
    interrupted.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_");
    assert!(
        parse_anthropic_sse(
            &interrupted,
            64 * 1024,
            64,
            &AnthropicToolBindings::default(),
            options()
        )
        .is_err()
    );
    assert_eq!(
        observed_anthropic_receipt(&interrupted, 64 * 1024, 64, options()),
        Some((response_id, usage))
    );
    assert!(observed_anthropic_receipt(&wire(&events[..4]), 64 * 1024, 64, options()).is_none());
    for invalid_usage in [
        json!({}),
        json!({"output_tokens":null}),
        json!({"output_tokens":-1}),
    ] {
        let mut invalid = events.to_vec();
        invalid[4]["usage"] = invalid_usage;
        assert!(observed_anthropic_receipt(&wire(&invalid), 64 * 1024, 64, options()).is_none());
    }
}

#[test]
fn observed_anthropic_usage_survives_framing_limits_without_stream_success() {
    let prefix = wire(&[start(), finish()]);
    let observed = observed_anthropic_receipt(&prefix, 2048, 2, options()).unwrap();
    for suffix in [
        format!("data: {}\n\n", "x".repeat(2049)),
        "data: {}\n\ndata: {}\n\n".into(),
    ] {
        let mut bytes = prefix.clone();
        bytes.extend_from_slice(suffix.as_bytes());
        assert_eq!(
            observed_anthropic_receipt(&bytes, 2048, 2, options()),
            Some(observed.clone())
        );
        let error = parse_anthropic_sse(
            &bytes,
            2048,
            2,
            &AnthropicToolBindings::default(),
            options(),
        )
        .err()
        .expect("framing violation cannot succeed");
        assert_eq!(error.kind(), AnthropicCodecErrorKind::SizeLimit);
    }
    assert!(observed_anthropic_receipt(&prefix, 2048, 1, options()).is_none());
    let mut duplicate = wire(&[start(), finish(), start()]);
    duplicate.extend_from_slice(format!("data: {}\n\n", "x".repeat(2049)).as_bytes());
    assert!(observed_anthropic_receipt(&duplicate, 2048, 8, options()).is_none());
    let mut invalid = prefix;
    invalid.push(0xff);
    assert!(observed_anthropic_receipt(&invalid, 2048, 2, options()).is_none());
}
