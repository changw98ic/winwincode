// SPDX-License-Identifier: Apache-2.0

//! `ChatGPT` Responses transport to the embedded Core's `ModelPort` event format.
//! Preserve response item IDs, message phase and encrypted reasoning for later turns.

use crate::{
    CanonicalModelStreamFrame, CredentialOutputBoundary, HttpsSseProviderCompletion,
    HttpsSseProviderError, HttpsSseProviderErrorKind, ModelAttemptFailureFact,
    ModelExecutionCertainty, ProviderFinishReason, ProviderGatewayOpenReceipt,
    ProviderGatewayTerminal, ProviderGatewayTerminalCharge, ProviderStreamConverter,
    ProviderStreamEvent, ProviderStreamFailure, ProviderStreamFailureKind, ProviderTokenUsage,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn protocol() -> HttpsSseProviderError {
    HttpsSseProviderError::new(HttpsSseProviderErrorKind::SseEvent)
}

pub(crate) fn prepare_request(
    payload: &[u8],
    model: &str,
) -> Result<Vec<u8>, HttpsSseProviderError> {
    prepare(payload, model, true)
}

pub(crate) fn prepare_plan_request(
    payload: &[u8],
    model: &str,
) -> Result<Vec<u8>, HttpsSseProviderError> {
    prepare(payload, model, false)
}

fn prepare(
    payload: &[u8],
    model: &str,
    codex_backend: bool,
) -> Result<Vec<u8>, HttpsSseProviderError> {
    let envelope: Value = serde_json::from_slice(payload).map_err(|_| protocol())?;
    let mut request = envelope
        .get("request")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(protocol)?;
    if request.get("model").and_then(Value::as_str) != Some(model)
        || !request.get("input").is_some_and(Value::is_array)
    {
        return Err(protocol());
    }
    request.insert("stream".into(), json!(true));
    request.insert("store".into(), json!(false));
    request.entry("instructions").or_insert_with(|| json!(""));
    // The subscription endpoint rejects API-only sampling/output-limit parameters.
    if codex_backend {
        for key in ["max_output_tokens", "temperature", "top_p"] {
            request.remove(key);
        }
    }
    serde_json::to_vec(&request).map_err(|_| protocol())
}

pub(crate) fn parse_response(
    bytes: &[u8],
    receipt: &ProviderGatewayOpenReceipt,
    max_event_bytes: usize,
    max_events: usize,
) -> Result<HttpsSseProviderCompletion, HttpsSseProviderError> {
    let mut stream = Responses::new(receipt);
    for event in response_events(bytes, max_event_bytes, max_events)? {
        stream.push(&event)?;
    }
    stream.finish()
}

fn response_events(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
) -> Result<Vec<Value>, HttpsSseProviderError> {
    response_frames(bytes, max_event_bytes, max_events)?
        .into_iter()
        .map(|frame| serde_json::from_str(&frame.data).map_err(|_| protocol()))
        .collect()
}

fn response_frames(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
) -> Result<Vec<crate::provider_sse_framing::SseFrame>, HttpsSseProviderError> {
    crate::provider_sse_framing::parse(bytes, max_event_bytes, max_events).map_err(|error| {
        HttpsSseProviderError::new(match error {
            crate::provider_sse_framing::SseFramingError::Utf8 => {
                HttpsSseProviderErrorKind::SseEvent
            }
            crate::provider_sse_framing::SseFramingError::SizeLimit => {
                HttpsSseProviderErrorKind::SizeLimit
            }
        })
    })
}

pub(crate) fn observed_receipt(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
) -> Option<(String, ProviderTokenUsage)> {
    let mut id = None;
    let mut usage = None;
    let bytes = match std::str::from_utf8(bytes) {
        Ok(_) => bytes,
        Err(error) if error.error_len().is_none() => &bytes[..error.valid_up_to()],
        Err(_) => return None,
    };
    for frame in
        crate::provider_sse_framing::parse_prefix(bytes, max_event_bytes, max_events).ok()?
    {
        let Ok(event) = serde_json::from_str::<Value>(&frame.data) else {
            break;
        };
        if let Some(response) = event.get("response") {
            if let Some(observed) = response.get("id").and_then(Value::as_str) {
                if observed.is_empty()
                    || observed.len() > 200
                    || observed.chars().any(char::is_control)
                    || id.as_deref().is_some_and(|previous| previous != observed)
                {
                    return None;
                }
                id = Some(observed.to_owned());
            }
            if response.get("usage").is_some_and(|value| !value.is_null()) {
                usage = Some(token_usage(response).ok()?);
            }
        }
    }
    Some((id?, usage?))
}

struct Responses<'a> {
    receipt: &'a ProviderGatewayOpenReceipt,
    frames: Vec<CanonicalModelStreamFrame>,
    response_id: Option<String>,
    items: BTreeSet<String>,
    done: BTreeSet<String>,
    terminal: Option<ProviderGatewayTerminal>,
    latest_usage: Option<ProviderTokenUsage>,
    output_observed: bool,
}

impl<'a> Responses<'a> {
    fn new(receipt: &'a ProviderGatewayOpenReceipt) -> Self {
        Self {
            receipt,
            frames: Vec::new(),
            response_id: None,
            items: BTreeSet::new(),
            done: BTreeSet::new(),
            terminal: None,
            latest_usage: None,
            output_observed: false,
        }
    }

    fn emit(&mut self, value: &Value, terminal: bool) -> Result<(), HttpsSseProviderError> {
        let sequence = u64::try_from(self.frames.len())
            .ok()
            .and_then(|value| value.checked_add(1))
            .filter(|value| *value <= 9_007_199_254_740_991)
            .ok_or_else(protocol)?;
        let payload = serde_json::to_string(&value).map_err(|_| protocol())?;
        self.receipt
            .stream_leak_gate
            .inspect_bytes(CredentialOutputBoundary::Event, payload.as_bytes())
            .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::CredentialLeak))?;
        self.frames
            .push(CanonicalModelStreamFrame::from_codex_event(
                sequence, payload, terminal,
            ));
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn push(&mut self, event: &Value) -> Result<(), HttpsSseProviderError> {
        if self.terminal.is_some() {
            return Err(protocol());
        }
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(protocol)?;
        if kind == "response.created" {
            if self.response_id.is_some() {
                return Err(protocol());
            }
            self.response_id = Some(string(event, "/response/id")?.to_owned());
            self.observe_usage(event)?;
            self.emit(&json!({"type":"created"}), false)?;
            return self.emit(
                &json!({"type":"server_model","model":self.receipt.route.model_id}),
                false,
            );
        }
        if matches!(kind, "response.failed" | "response.incomplete" | "error") {
            return self.fail(event);
        }
        if self.response_id.is_none() {
            return Err(protocol());
        }
        self.observe_usage(event)?;
        let mapped = match kind {
            "response.output_item.added" | "response.output_item.done" => {
                let item = event
                    .get("item")
                    .filter(|item| item.is_object())
                    .ok_or_else(protocol)?;
                let id = string(item, "/id")?.to_owned();
                if !matches!(
                    string(item, "/type")?,
                    "message" | "reasoning" | "function_call" | "custom_tool_call"
                ) {
                    return Err(protocol());
                }
                if kind == "response.output_item.added" {
                    if self.items.len() >= 4096 || !self.items.insert(id) {
                        return Err(protocol());
                    }
                } else if !self.items.contains(&id) || !self.done.insert(id) {
                    return Err(protocol());
                }
                Some(
                    json!({"type":if kind.ends_with("added"){"output_item_added"}else{"output_item_done"},"item":item}),
                )
            }
            "response.output_text.delta" => {
                Some(json!({"type":"output_text_delta","delta":string(event,"/delta")?}))
            }
            "response.custom_tool_call_input.delta" => Some(
                json!({"type":"tool_call_input_delta","itemId":string(event,"/item_id")?,"callId":event.get("call_id"),"delta":string(event,"/delta")?}),
            ),
            "response.reasoning_summary_part.added" => Some(
                json!({"type":"reasoning_summary_part_added","summaryIndex":number(event,"/summary_index")?}),
            ),
            "response.reasoning_summary_text.delta" => Some(
                json!({"type":"reasoning_summary_delta","summaryIndex":number(event,"/summary_index")?,"delta":string(event,"/delta")?}),
            ),
            "response.reasoning_summary_text.done" => Some(
                json!({"type":"reasoning_summary_done","summaryIndex":number(event,"/summary_index")?,"itemId":string(event,"/item_id")?,"text":string(event,"/text")?}),
            ),
            "response.reasoning_text.delta" => Some(
                json!({"type":"reasoning_content_delta","contentIndex":number(event,"/content_index")?,"delta":string(event,"/delta")?}),
            ),
            "response.completed" => {
                if Some(string(event, "/response/id")?) != self.response_id.as_deref()
                    || self.done.is_empty()
                    || self.done != self.items
                {
                    return Err(protocol());
                }
                let usage = token_usage(&event["response"])?;
                self.emit(&json!({"type":"completed","responseId":self.response_id,"tokenUsage":{
                    "input_tokens":usage.input_tokens,"cached_input_tokens":usage.cached_input_tokens,"cache_write_input_tokens":usage.cache_write_input_tokens,
                    "output_tokens":usage.output_tokens,"reasoning_output_tokens":usage.reasoning_output_tokens,"total_tokens":usage.input_tokens+usage.output_tokens},"endTurn":event.pointer("/response/end_turn")}),true)?;
                self.terminal = Some(ProviderGatewayTerminal::Completed {
                    usage,
                    actual_cost_micros: None,
                });
                None
            }
            // These events carry bookkeeping already present in completed response items.
            "response.in_progress"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.custom_tool_call_input.done"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_text.done" => None,
            _ => return Err(protocol()),
        };
        if let Some(value) = mapped {
            self.output_observed |=
                kind == "response.output_item.done" || value.get("delta").is_some();
            self.emit(&value, false)?;
        }
        Ok(())
    }

    fn observe_usage(&mut self, event: &Value) -> Result<(), HttpsSseProviderError> {
        let Some(response) = event
            .get("response")
            .filter(|response| response.get("usage").is_some_and(|value| !value.is_null()))
        else {
            return Ok(());
        };
        if Some(string(response, "/id")?) != self.response_id.as_deref() {
            return Err(protocol());
        }
        self.latest_usage = Some(token_usage(response)?);
        Ok(())
    }

    fn fail(&mut self, event: &Value) -> Result<(), HttpsSseProviderError> {
        let response = event.get("response");
        if let Some(id) = response
            .and_then(|response| response.get("id"))
            .and_then(Value::as_str)
        {
            if self
                .response_id
                .as_deref()
                .is_some_and(|previous| previous != id)
            {
                return Err(protocol());
            }
            self.response_id = Some(id.to_owned());
        }
        let code = response
            .and_then(|response| response.pointer("/error/code"))
            .or_else(|| event.pointer("/error/code"))
            .or_else(|| event.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let kind = match code {
            "invalid_api_key" | "authentication_error" | "unauthorized" => {
                ProviderStreamFailureKind::Authentication
            }
            "invalid_request_error" | "invalid_request" => {
                ProviderStreamFailureKind::InvalidRequest
            }
            "rate_limit_exceeded" | "rate_limit_error" => ProviderStreamFailureKind::RateLimit,
            "insufficient_quota" | "quota_exceeded" => ProviderStreamFailureKind::Quota,
            "context_length_exceeded" => ProviderStreamFailureKind::ContextWindowExceeded,
            "server_error" | "internal_error" => ProviderStreamFailureKind::Server,
            _ => ProviderStreamFailureKind::Unknown,
        };
        self.observe_usage(event)?;
        let usage = self.latest_usage;
        let mut converter = ProviderStreamConverter::from_gateway_receipt(self.receipt);
        if let Some(id) = &self.response_id {
            converter
                .ingest(ProviderStreamEvent::ResponseStarted {
                    provider_response_id: id.clone(),
                    observed_model_id: None,
                })
                .map_err(|_| protocol())?;
        }
        if let Some(usage) = usage {
            converter
                .ingest(ProviderStreamEvent::Usage(usage))
                .map_err(|_| protocol())?;
        }
        let mut failure = ProviderStreamFailure::new(kind);
        if let Some(id) = &self.response_id {
            failure = failure.with_provider_request_id(id.clone());
        }
        let incomplete = response
            .and_then(|response| response.pointer("/incomplete_details/reason"))
            .and_then(Value::as_str);
        let terminal_event = match incomplete {
            Some("max_output_tokens") => {
                ProviderStreamEvent::Finished(ProviderFinishReason::MaxTokens)
            }
            Some("content_filter") => ProviderStreamEvent::Finished(ProviderFinishReason::Filtered),
            _ => ProviderStreamEvent::Failed(failure),
        };
        let frames = converter.ingest(terminal_event).map_err(|error| {
            if error.kind() == crate::ProviderStreamConversionErrorKind::CredentialLeak {
                HttpsSseProviderError::new(HttpsSseProviderErrorKind::CredentialLeak)
            } else {
                protocol()
            }
        })?;
        let terminal = frames.last().ok_or_else(protocol)?;
        self.emit(
            &serde_json::from_str(terminal.payload_json()).map_err(|_| protocol())?,
            true,
        )?;
        self.terminal = Some(ProviderGatewayTerminal::Failed {
            failure: ModelAttemptFailureFact::from_stream(
                kind,
                if self.output_observed {
                    ModelExecutionCertainty::OutputObserved
                } else {
                    ModelExecutionCertainty::AcceptanceUnknown
                },
            ),
            charge: usage.map(|usage| ProviderGatewayTerminalCharge {
                usage,
                actual_cost_micros: None,
            }),
        });
        Ok(())
    }

    fn finish(self) -> Result<HttpsSseProviderCompletion, HttpsSseProviderError> {
        if self.terminal.is_none() {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::IncompleteStream,
            ));
        }
        Ok(HttpsSseProviderCompletion {
            frames: self.frames,
            terminal: self.terminal.ok_or_else(protocol)?,
        })
    }
}

fn token_usage(response: &Value) -> Result<ProviderTokenUsage, HttpsSseProviderError> {
    let usage = ProviderTokenUsage {
        input_tokens: number(response, "/usage/input_tokens")?,
        output_tokens: number(response, "/usage/output_tokens")?,
        cached_input_tokens: optional_cache_number(
            response,
            "/usage/input_tokens_details/cached_tokens",
        )?,
        cache_write_input_tokens: optional_number(
            response,
            "/usage/input_tokens_details/cache_write_tokens",
        )?,
        reasoning_output_tokens: optional_number(
            response,
            "/usage/output_tokens_details/reasoning_tokens",
        )?,
    };
    if usage
        .cached_input_tokens
        .is_some_and(|cached| cached > usage.input_tokens)
        || usage.reasoning_output_tokens > usage.output_tokens
        || usage
            .input_tokens
            .checked_add(usage.output_tokens)
            .is_none_or(|total| total > 9_007_199_254_740_991)
    {
        return Err(protocol());
    }
    Ok(usage)
}

fn string<'a>(value: &'a Value, path: &str) -> Result<&'a str, HttpsSseProviderError> {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .ok_or_else(protocol)
}
fn number(value: &Value, path: &str) -> Result<u64, HttpsSseProviderError> {
    value
        .pointer(path)
        .and_then(Value::as_u64)
        .filter(|value| *value <= 9_007_199_254_740_991)
        .ok_or_else(protocol)
}
fn optional_cache_number(value: &Value, path: &str) -> Result<Option<u64>, HttpsSseProviderError> {
    value.pointer(path).map(|_| number(value, path)).transpose()
}
fn optional_number(value: &Value, path: &str) -> Result<u64, HttpsSseProviderError> {
    if value.pointer(path).is_none() {
        Ok(0)
    } else {
        number(value, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CredentialLeakGate, ResolvedSecret};
    use winwincode_api::generated::ModelRoute;
    use winwincode_domain::{CredentialReferenceId, ModelExchangeId, RequestId};

    fn receipt() -> ProviderGatewayOpenReceipt {
        ProviderGatewayOpenReceipt {
            model_exchange_id: ModelExchangeId("mdl_00000000000000000000000001".into()),
            request_id: RequestId("req_00000000000000000000000001".into()),
            route: ModelRoute {
                provider_id: "codex".into(),
                model_id: "test-model".into(),
                credential_reference_id: CredentialReferenceId(
                    "crd_00000000000000000000000001".into(),
                ),
            },
            adapter_request_id: "probe".into(),
            idempotent_replay: false,
            stream_leak_gate: CredentialLeakGate::new(),
        }
    }
    fn wire(events: &[Value]) -> Vec<u8> {
        use std::fmt::Write as _;
        let mut text = String::new();
        for event in events {
            writeln!(
                &mut text,
                "event: {}\ndata: {event}\n",
                event["type"].as_str().unwrap()
            )
            .unwrap();
        }
        text.into_bytes()
    }
    fn response_events() -> Vec<Value> {
        vec![
            json!({"type":"response.created","response":{"id":"resp-one"}}),
            json!({"type":"response.output_item.added","item":{"type":"reasoning","id":"rs-one","summary":[]}}),
            json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"rs-one","summary":[],"encrypted_content":"encrypted-reasoning"}}),
            json!({"type":"response.output_item.added","item":{"type":"message","id":"msg-one","role":"assistant","content":[],"phase":"final_answer"}}),
            json!({"type":"response.output_item.done","item":{"type":"message","id":"msg-one","role":"assistant","content":[{"type":"output_text","text":"OK"}],"phase":"final_answer"}}),
            json!({"type":"response.completed","response":{"id":"resp-one","usage":{"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":1}},"end_turn":true}}),
        ]
    }
    #[test]
    fn responses_frames_enter_the_typed_model_replay_stream() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let template = fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.chunk")
            .unwrap();
        let completion = parse_response(&wire(&response_events()), &receipt(), 4096, 100).unwrap();
        for (index, frame) in completion.frames.iter().enumerate() {
            let mut chunk: winwincode_execution_port::generated::ModelChunkMessage =
                serde_json::from_value(template.clone()).unwrap();
            chunk.sequence =
                winwincode_domain::ExecutionSequence(i64::try_from(frame.sequence()).unwrap());
            chunk.payload = Some(frame.encoded_payload());
            chunk.error = None;
            chunk.is_final = frame.is_terminal();
            let mapped = winwincode_execution_port::typed_replay::frame_from_message(
                &winwincode_execution_port::generated::ExecutionPortMessage::ModelChunkMessage(
                    chunk,
                ),
            )
            .expect("Responses frames must satisfy the actual replay ingress contract");
            assert_eq!(mapped.frame.sequence, u64::try_from(index).unwrap() + 1);
        }
        assert!(completion.frames.last().unwrap().is_terminal());
    }

    #[test]
    fn responses_failures_preserve_retry_classification_and_paid_usage() {
        for (code, expected) in [
            ("invalid_api_key", "AUTH"),
            ("invalid_request_error", "INVALID_REQUEST"),
            ("rate_limit_exceeded", "RATE_LIMIT"),
            ("insufficient_quota", "QUOTA"),
            ("context_length_exceeded", "CONTEXT_WINDOW_EXCEEDED"),
            ("server_error", "SERVER"),
        ] {
            let mut events = response_events();
            *events.last_mut().unwrap() = json!({"type":"response.failed","response":{
                "id":"resp-one","error":{"code":code,"message":"untrusted provider message"},
                "usage":{"input_tokens":10,"output_tokens":4}}});
            let completion = parse_response(&wire(&events), &receipt(), 4096, 100).unwrap();
            let value: Value =
                serde_json::from_str(completion.frames.last().unwrap().payload_json()).unwrap();
            assert_eq!(value["error"]["code"], expected);
            assert!(!value.to_string().contains("untrusted provider message"));
            assert_eq!(value["tokenUsage"]["input_tokens"], 10);
            assert!(
                value["error"]["providerRequestId"]
                    .as_str()
                    .unwrap()
                    .starts_with("sha256:")
            );
            let ProviderGatewayTerminal::Failed { failure, charge } = completion.terminal else {
                panic!("failed response");
            };
            assert_eq!(failure.certainty, ModelExecutionCertainty::OutputObserved);
            assert_eq!(charge.unwrap().usage.output_tokens, 4);
            events.last_mut().unwrap()["response"]["id"] = json!("other-response");
            assert!(parse_response(&wire(&events), &receipt(), 4096, 100).is_err());
        }
    }

    #[test]
    fn responses_preserve_phase_encrypted_reasoning_and_usage() {
        let completion = parse_response(&wire(&response_events()), &receipt(), 4096, 100).unwrap();
        assert_eq!(
            completion.terminal.outcome(),
            crate::ProviderGatewayTerminalOutcome::Succeeded
        );
        let payloads = completion
            .frames
            .iter()
            .map(CanonicalModelStreamFrame::payload_json)
            .collect::<String>();
        assert!(payloads.contains("encrypted-reasoning"));
        assert!(payloads.contains("final_answer"));
        let last: Value =
            serde_json::from_str(completion.frames.last().unwrap().payload_json()).unwrap();
        assert_eq!(last["tokenUsage"]["cached_input_tokens"], 2);
        assert_eq!(last["tokenUsage"]["total_tokens"], 14);
        assert!(completion.frames.last().unwrap().is_terminal());
    }
    #[test]
    fn missing_cache_usage_and_actual_cost_remain_unknown() {
        let mut events = response_events();
        events.last_mut().unwrap()["response"]["usage"] =
            json!({"input_tokens":10,"output_tokens":4});
        let completion = parse_response(&wire(&events), &receipt(), 4096, 100).unwrap();
        let ProviderGatewayTerminal::Completed {
            usage,
            actual_cost_micros,
        } = completion.terminal
        else {
            panic!("expected a completed response");
        };
        assert_eq!(usage.cached_input_tokens, None);
        assert_eq!(actual_cost_micros, None);
    }
    #[test]
    fn observed_responses_usage_does_not_require_a_successful_terminal() {
        let events = vec![
            json!({"type":"response.created","response":{"id":"resp-one"}}),
            json!({"type":"response.in_progress","response":{"id":"resp-one","usage":{"input_tokens":10,"output_tokens":4}}}),
        ];
        let observed = observed_receipt(&wire(&events), 4096, 100).unwrap();
        assert_eq!(observed.0, "resp-one");
        assert_eq!(observed.1.input_tokens, 10);
        assert_eq!(observed.1.cached_input_tokens, None);
        let mut cut = wire(&events);
        cut.extend_from_slice(b"data: {\"type\":\"response.out");
        cut.extend_from_slice(&[0xe4, 0xb8]);
        assert_eq!(observed_receipt(&cut, 4096, 100), Some(observed));
        let mut drifted = events;
        drifted.last_mut().unwrap()["response"]["id"] = json!("other-response");
        assert!(observed_receipt(&wire(&drifted), 4096, 100).is_none());
    }

    #[test]
    fn observed_responses_usage_survives_framing_limits_without_stream_success() {
        let events = vec![
            json!({"type":"response.created","response":{"id":"resp-one"}}),
            json!({"type":"response.in_progress","response":{"id":"resp-one","usage":{"input_tokens":10,"output_tokens":4}}}),
        ];
        let prefix = wire(&events);
        let observed = observed_receipt(&prefix, 4096, 2).unwrap();
        for suffix in [
            format!("data: {}\n\n", "x".repeat(4097)),
            "data: {}\n\ndata: {}\n\n".into(),
        ] {
            let mut bytes = prefix.clone();
            bytes.extend_from_slice(suffix.as_bytes());
            assert_eq!(observed_receipt(&bytes, 4096, 2), Some(observed.clone()));
            assert_eq!(
                parse_response(&bytes, &receipt(), 4096, 2)
                    .unwrap_err()
                    .kind(),
                HttpsSseProviderErrorKind::SizeLimit
            );
        }
        assert!(
            observed_receipt(&prefix, 4096, 1).is_none(),
            "usage beyond the frame limit is not observed"
        );
        let mut drifted = events;
        drifted.push(json!({"type":"response.in_progress","response":{"id":"other-response"}}));
        let mut bytes = wire(&drifted);
        bytes.extend_from_slice(format!("data: {}\n\n", "x".repeat(4097)).as_bytes());
        assert!(observed_receipt(&bytes, 4096, 8).is_none());
        let mut invalid = prefix;
        invalid.push(0xff);
        assert!(observed_receipt(&invalid, 4096, 2).is_none());
    }

    #[test]
    fn cutoff_identity_drift_and_echoed_credentials_fail_closed() {
        let mut events = response_events();
        events.pop();
        assert_eq!(
            parse_response(&wire(&events), &receipt(), 4096, 100)
                .unwrap_err()
                .kind(),
            HttpsSseProviderErrorKind::IncompleteStream
        );
        assert!(
            parse_response(&wire(&events), &receipt(), 4096, 100)
                .unwrap_err()
                .retryable()
        );
        events = response_events();
        events.last_mut().unwrap()["response"]["id"] = json!("other-response");
        assert!(parse_response(&wire(&events), &receipt(), 4096, 100).is_err());
        events = response_events();
        let mut receipt = receipt();
        receipt.stream_leak_gate.track_secret(
            &ResolvedSecret::from_bytes(b"private-login-test-token".to_vec()).unwrap(),
        );
        events[4]["item"]["content"][0]["text"] = json!("private-login-test-token");
        assert_eq!(
            parse_response(&wire(&events), &receipt, 4096, 100)
                .unwrap_err()
                .kind(),
            HttpsSseProviderErrorKind::CredentialLeak
        );
    }
    #[test]
    fn request_unwraps_envelope_and_requires_frozen_model() {
        let input = json!({"request":{"model":"test-model","input":[],"max_output_tokens":12,"store":true}});
        let payload = serde_json::to_vec(&input).unwrap();
        let request: Value =
            serde_json::from_slice(&prepare_request(&payload, "test-model").unwrap()).unwrap();
        assert_eq!(request["store"], false);
        assert_eq!(request["stream"], true);
        assert!(request.get("max_output_tokens").is_none());
        assert!(prepare_request(&payload, "other-model").is_err());
    }
}
