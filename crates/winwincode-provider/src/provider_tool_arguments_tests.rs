// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};
use std::fmt::Write;
use winwincode_api::generated::ModelRoute;
use winwincode_domain::{CredentialReferenceId, ModelExchangeId, RequestId};

use crate::provider_anthropic::{
    AnthropicCodecError, AnthropicMessagesOptions, PreparedAnthropicRequest, ProviderTokenPricing,
    parse_anthropic_sse, prepare_anthropic_request,
};
use crate::{ProviderGatewayTerminal, ProviderStreamEvent, ProviderTokenUsage, ProviderToolKind};

#[derive(Clone, Copy)]
enum Protocol {
    Anthropic,
    Chat,
}

fn options() -> AnthropicMessagesOptions {
    AnthropicMessagesOptions {
        max_output_tokens: 4096,
        pricing: ProviderTokenPricing::default(),
    }
}

fn request(namespace: Option<&str>) -> Value {
    let custom = json!({"type":"custom","name":"exec","description":"Execute fixture code",
        "format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}});
    let tool = namespace.map_or_else(
        || custom.clone(),
        |name| json!({"type":"namespace","name":name,"description":"Fixture namespace","tools":[custom]}),
    );
    json!({"requestId":"request-argument-fixture","provider":"fixture-provider",
        "sessionId":"session-fixture","threadId":"thread-fixture","turnId":"turn-fixture",
        "request":{"model":"fixture-model","instructions":"Use the fixture tools.","input":[{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"Run the fixture"}]}],
            "tools":[tool],"tool_choice":"auto","parallel_tool_calls":true,
            "stream":true,"store":false,"include":[],"reasoning":null,"text":null}})
}

fn prepare(
    protocol: Protocol,
    request: &Value,
) -> Result<PreparedAnthropicRequest, AnthropicCodecError> {
    let bytes = serde_json::to_vec(request).unwrap();
    match protocol {
        Protocol::Anthropic => prepare_anthropic_request(&bytes, "fixture-model", options()),
        Protocol::Chat => {
            crate::provider_openai::prepare_openai_chat_request(&bytes, "fixture-model", options())
        }
    }
}

fn exposed_name(protocol: Protocol, prepared: &PreparedAnthropicRequest) -> String {
    let body: Value = serde_json::from_slice(&prepared.body).unwrap();
    match protocol {
        Protocol::Anthropic => body["tools"][0]["name"].as_str().unwrap(),
        Protocol::Chat => body["tools"][0]["function"]["name"].as_str().unwrap(),
    }
    .to_owned()
}

fn wire(protocol: Protocol, calls: &[(&str, &str, &str)]) -> Vec<u8> {
    let mut frames = Vec::new();
    match protocol {
        Protocol::Anthropic => {
            frames.push(json!({"type":"message_start","message":{"id":"response-fixture","type":"message","role":"assistant",
                "usage":{"input_tokens":3,"output_tokens":0}}}));
            for (index, (call, name, arguments)) in calls.iter().enumerate() {
                frames.extend([
                    json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":call,"name":name,"input":{}}}),
                    json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":arguments}}),
                    json!({"type":"content_block_stop","index":index}),
                ]);
            }
            frames.extend([
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":7}}),
                json!({"type":"message_stop"}),
            ]);
        }
        Protocol::Chat => {
            for (index, (call, name, arguments)) in calls.iter().enumerate() {
                frames.push(json!({"id":"response-fixture","object":"chat.completion.chunk",
                    "choices":[{"index":0,"delta":{"tool_calls":[{"index":index,"id":call,"type":"function",
                    "function":{"name":name,"arguments":arguments}}]},"finish_reason":null}]}));
            }
            frames.push(json!({"id":"response-fixture","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":3,"completion_tokens":7,"total_tokens":10}}));
        }
    }
    let mut body = String::new();
    for frame in frames {
        if matches!(protocol, Protocol::Anthropic) {
            writeln!(body, "event: {}", frame["type"].as_str().unwrap()).unwrap();
        }
        writeln!(body, "data: {frame}\n").unwrap();
    }
    if matches!(protocol, Protocol::Chat) {
        body.push_str("data: [DONE]\n\n");
    }
    body.into_bytes()
}

fn parse(
    protocol: Protocol,
    prepared: &PreparedAnthropicRequest,
    calls: &[(&str, &str, &str)],
) -> Result<(Vec<ProviderStreamEvent>, ProviderGatewayTerminal), AnthropicCodecError> {
    let body = wire(protocol, calls);
    match protocol {
        Protocol::Anthropic => {
            parse_anthropic_sse(&body, 64 * 1024, 128, &prepared.tool_bindings, options())
                .map(|stream| (stream.events, stream.terminal))
        }
        Protocol::Chat => crate::provider_openai::parse_openai_chat_sse(
            &body,
            64 * 1024,
            128,
            &prepared.tool_bindings,
            options(),
        )
        .map(|stream| (stream.events, stream.terminal)),
    }
}

fn assert_call(
    events: &[ProviderStreamEvent],
    call_id: &str,
    kind: ProviderToolKind,
    namespace: Option<&str>,
    arguments: &str,
) {
    let starts: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ProviderStreamEvent::ToolCallStarted {
                provider_call_id,
                identity,
                ..
            } if provider_call_id == call_id => Some(identity),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].kind(), kind);
    assert_eq!(starts[0].name(), "exec");
    assert_eq!(starts[0].namespace(), namespace);
    let actual: String = events
        .iter()
        .filter_map(|event| match event {
            ProviderStreamEvent::ToolCallArgumentsDelta {
                provider_call_id,
                delta,
                ..
            } if provider_call_id == call_id => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    if kind == ProviderToolKind::Function {
        assert_eq!(
            serde_json::from_str::<Value>(&actual).unwrap(),
            serde_json::from_str::<Value>(arguments).unwrap()
        );
    } else {
        assert_eq!(actual, arguments);
    }
    assert_eq!(events.iter().filter(|event| matches!(event, ProviderStreamEvent::ToolCallEnded { provider_call_id, .. } if provider_call_id == call_id)).count(), 1);
}

fn assert_usage(protocol: Protocol, terminal: &ProviderGatewayTerminal) {
    assert_eq!(
        terminal,
        &ProviderGatewayTerminal::Completed {
            usage: ProviderTokenUsage {
                input_tokens: 3,
                cached_input_tokens: match protocol {
                    Protocol::Anthropic => Some(0),
                    Protocol::Chat => None,
                },
                cache_write_input_tokens: 0,
                output_tokens: 7,
                reasoning_output_tokens: 0
            },
            actual_cost_micros: None,
        }
    );
}

#[test]
fn tool_argument_admission_bad_objects_reach_function_feedback_without_promoting_authority() {
    for protocol in [Protocol::Anthropic, Protocol::Chat] {
        let prepared = prepare(protocol, &request(None)).unwrap();
        let name = exposed_name(protocol, &prepared);
        for bad in [
            json!({"command":"fixture","workdir":"fixture"}),
            json!({"arguments":"fixture"}),
            json!({"cmd":"fixture"}),
            json!({"input":[{"kind":"fixture","data":{}}]}),
            json!({"input":42}),
            json!({"input":null}),
            json!({"input":"fixture","extra":true}),
        ] {
            let arguments = serde_json::to_string(&bad).unwrap();
            let (events, terminal) = parse(protocol, &prepared, &[("bad-call", &name, &arguments)])
                .expect("valid object reaches Core payload-kind rejection");
            assert_call(
                &events,
                "bad-call",
                ProviderToolKind::Function,
                None,
                &arguments,
            );
            assert_usage(protocol, &terminal);
        }
    }
}

#[test]
fn tool_argument_admission_correct_successor_keeps_custom_identity_and_input() {
    for protocol in [Protocol::Anthropic, Protocol::Chat] {
        let prepared = prepare(protocol, &request(None)).unwrap();
        let name = exposed_name(protocol, &prepared);
        let (events, terminal) = parse(
            protocol,
            &prepared,
            &[
                ("bad-call", &name, "{\"arguments\":\"fixture\"}"),
                ("good-call", &name, "{\"input\":\"text('fixture')\"}"),
            ],
        )
        .unwrap();
        assert_call(
            &events,
            "bad-call",
            ProviderToolKind::Function,
            None,
            "{\"arguments\":\"fixture\"}",
        );
        assert_call(
            &events,
            "good-call",
            ProviderToolKind::Custom,
            None,
            "text('fixture')",
        );
        assert_usage(protocol, &terminal);
    }
}

#[test]
fn tool_argument_admission_preserves_explicit_namespace_for_bad_and_good_calls() {
    for protocol in [Protocol::Anthropic, Protocol::Chat] {
        let prepared = prepare(protocol, &request(Some("fixture_space"))).unwrap();
        let name = exposed_name(protocol, &prepared);
        let (events, terminal) = parse(
            protocol,
            &prepared,
            &[
                ("bad-call", &name, "{\"cmd\":\"fixture\"}"),
                ("good-call", &name, "{\"input\":\"text('fixture')\"}"),
            ],
        )
        .unwrap();
        assert_call(
            &events,
            "bad-call",
            ProviderToolKind::Function,
            Some("fixture_space"),
            "{\"cmd\":\"fixture\"}",
        );
        assert_call(
            &events,
            "good-call",
            ProviderToolKind::Custom,
            Some("fixture_space"),
            "text('fixture')",
        );
        assert_usage(protocol, &terminal);
    }
}

#[test]
fn tool_argument_admission_empty_custom_input_reaches_core_without_empty_delta() {
    for protocol in [Protocol::Anthropic, Protocol::Chat] {
        let prepared = prepare(protocol, &request(None)).unwrap();
        let name = exposed_name(protocol, &prepared);
        let (events, terminal) = parse(
            protocol,
            &prepared,
            &[("empty-call", &name, "{\"input\":\"\"}")],
        )
        .unwrap();
        assert_call(&events, "empty-call", ProviderToolKind::Custom, None, "");
        assert!(!events.iter().any(|event| matches!(event, ProviderStreamEvent::ToolCallArgumentsDelta { provider_call_id, .. } if provider_call_id == "empty-call")));
        assert_usage(protocol, &terminal);
    }
}

#[test]
fn tool_argument_admission_malformed_json_and_non_objects_remain_protocol_failures() {
    for protocol in [Protocol::Anthropic, Protocol::Chat] {
        let prepared = prepare(protocol, &request(None)).unwrap();
        let name = exposed_name(protocol, &prepared);
        for arguments in ["{", "[]", "null", "42", "\"fixture\""] {
            assert!(parse(protocol, &prepared, &[("invalid-call", &name, arguments)]).is_err());
        }
    }
}

#[test]
fn tool_argument_admission_unknown_name_never_gains_advertised_custom_identity() {
    for protocol in [Protocol::Anthropic, Protocol::Chat] {
        let prepared = prepare(protocol, &request(Some("fixture_space"))).unwrap();
        let (events, terminal) = parse(
            protocol,
            &prepared,
            &[("unknown-call", "exec", "{\"input\":\"fixture\"}")],
        )
        .unwrap();
        assert_call(
            &events,
            "unknown-call",
            ProviderToolKind::Function,
            Some("winwincode_unadvertised"),
            "{\"input\":\"fixture\"}",
        );
        assert_usage(protocol, &terminal);
    }
}

#[test]
fn tool_argument_admission_replays_exact_function_history_for_declared_custom_binding() {
    for protocol in [Protocol::Anthropic, Protocol::Chat] {
        for namespace in [None, Some("fixture_space")] {
            let mut original = request(namespace);
            let mut call = json!({"type":"function_call","call_id":"bad-call","name":"exec","arguments":"{\"input\":[{\"kind\":\"fixture\",\"data\":{}}]}"});
            if let Some(namespace) = namespace {
                call["namespace"] = json!(namespace);
            }
            original["request"]["input"].as_array_mut().unwrap().extend([call,json!({"type":"function_call_output","call_id":"bad-call","output":"tool invoked with incompatible payload"})]);
            let prepared = prepare(protocol, &original)
                .expect("replay stored rejected Function history without granting execution");
            let body: Value = serde_json::from_slice(&prepared.body).unwrap();
            let matching: Vec<_> = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "assistant")
                .collect();
            assert_eq!(matching.len(), 1);
            let arguments = match protocol {
                Protocol::Anthropic => matching[0]["content"][0]["input"].clone(),
                Protocol::Chat => serde_json::from_str(
                    matching[0]["tool_calls"][0]["function"]["arguments"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap(),
            };
            assert_eq!(arguments, json!({"input":[{"kind":"fixture","data":{}}]}));
            let name = exposed_name(protocol, &prepared);
            assert_eq!(
                prepared.tool_bindings.identity(&name).unwrap().kind(),
                ProviderToolKind::Custom
            );
        }
    }
}

#[test]
fn tool_argument_admission_history_cannot_swap_namespace_or_invent_tool_authority() {
    for protocol in [Protocol::Anthropic, Protocol::Chat] {
        let mut original = request(Some("fixture_space"));
        original["request"]["input"].as_array_mut().unwrap().push(json!({"type":"function_call","call_id":"bad-call","name":"exec","namespace":"different_space","arguments":"{}"}));
        assert!(prepare(protocol, &original).is_err());
    }
}

#[test]
fn tool_argument_admission_duplicate_kind_cannot_create_an_alternate_handler() {
    for protocol in [Protocol::Anthropic, Protocol::Chat] {
        for namespace in [None, Some("fixture_space")] {
            let mut original = request(namespace);
            let function = json!({"type":"function","name":"exec","description":"Fixture function",
                "parameters":{"type":"object","properties":{},"additionalProperties":false}});
            if namespace.is_some() {
                original["request"]["tools"][0]["tools"]
                    .as_array_mut()
                    .unwrap()
                    .push(function);
            } else {
                original["request"]["tools"]
                    .as_array_mut()
                    .unwrap()
                    .push(function);
            }
            assert!(prepare(protocol, &original).is_err());
        }
    }
}

#[test]
fn tool_argument_admission_converter_preserves_added_done_kind_and_single_completion() {
    for protocol in [Protocol::Anthropic, Protocol::Chat] {
        let prepared = prepare(protocol, &request(Some("fixture_space"))).unwrap();
        let name = exposed_name(protocol, &prepared);
        let (events, terminal) = parse(
            protocol,
            &prepared,
            &[
                (
                    "bad-call",
                    &name,
                    "{\"input\":[{\"kind\":\"fixture\",\"data\":{}}]}",
                ),
                ("good-call", &name, "{\"input\":\"text('fixture')\"}"),
                ("empty-call", &name, "{\"input\":\"\"}"),
            ],
        )
        .unwrap();
        assert_usage(protocol, &terminal);
        let receipt = crate::ProviderGatewayOpenReceipt {
            model_exchange_id: ModelExchangeId("mdl_00000000000000000000000001".into()),
            request_id: RequestId("req_00000000000000000000000001".into()),
            route: ModelRoute {
                provider_id: "fixture-provider".into(),
                model_id: "fixture-model".into(),
                credential_reference_id: CredentialReferenceId(
                    "crd_00000000000000000000000001".into(),
                ),
            },
            adapter_request_id: "fixture-attempt".into(),
            idempotent_replay: false,
            stream_leak_gate: crate::CredentialLeakGate::new(),
        };
        let mut converter = crate::ProviderStreamConverter::from_gateway_receipt(&receipt);
        let frames: Vec<Value> = events
            .into_iter()
            .flat_map(|event| converter.ingest(event).unwrap())
            .map(|frame| serde_json::from_str(frame.payload_json()).unwrap())
            .collect();
        for (call, kind, field, expected) in [
            (
                "bad-call",
                "function_call",
                "arguments",
                "{\"input\":[{\"kind\":\"fixture\",\"data\":{}}]}",
            ),
            ("good-call", "custom_tool_call", "input", "text('fixture')"),
            ("empty-call", "custom_tool_call", "input", ""),
        ] {
            let added: Vec<_> = frames
                .iter()
                .filter(|f| f["type"] == "output_item_added" && f["item"]["call_id"] == call)
                .collect();
            let done: Vec<_> = frames
                .iter()
                .filter(|f| f["type"] == "output_item_done" && f["item"]["call_id"] == call)
                .collect();
            assert_eq!((added.len(), done.len()), (1, 1));
            assert_eq!(added[0]["item"]["type"], kind);
            assert_eq!(done[0]["item"]["type"], kind);
            assert_eq!(added[0]["item"]["namespace"], "fixture_space");
            assert_eq!(done[0]["item"]["namespace"], "fixture_space");
            assert_eq!(added[0]["item"]["id"], done[0]["item"]["id"]);
            if kind == "function_call" {
                assert_eq!(
                    serde_json::from_str::<Value>(done[0]["item"][field].as_str().unwrap())
                        .unwrap(),
                    serde_json::from_str::<Value>(expected).unwrap()
                );
            } else {
                assert_eq!(done[0]["item"][field], expected);
            }
        }
        let completed: Vec<_> = frames.iter().filter(|f| f["type"] == "completed").collect();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0]["tokenUsage"]["input_tokens"], 3);
        assert_eq!(completed[0]["tokenUsage"]["output_tokens"], 7);
        assert_eq!(frames.last(), completed.first().copied());
    }
}
