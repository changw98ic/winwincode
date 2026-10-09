// SPDX-License-Identifier: Apache-2.0

//! Responses transport to the embedded Core's `ModelPort` event format.
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
    prepare(payload, model, true, None)
}

pub(crate) fn prepare_plan_request(
    payload: &[u8],
    model: &str,
) -> Result<Vec<u8>, HttpsSseProviderError> {
    prepare(payload, model, false, None)
}

pub(crate) fn prepare_api_request(
    payload: &[u8],
    model: &str,
    max_output_tokens: u32,
) -> Result<Vec<u8>, HttpsSseProviderError> {
    if max_output_tokens == 0 {
        return Err(protocol());
    }
    prepare(payload, model, false, Some(max_output_tokens))
}

pub(crate) fn response_schema(
    payload: &[u8],
) -> Result<Option<crate::provider_response_schema::ResponseSchema>, HttpsSseProviderError> {
    let source = std::str::from_utf8(payload).map_err(|_| protocol())?;
    if crate::provider_response_schema::reserved_object_key(source) {
        return Err(protocol());
    }
    let envelope: Value = serde_json::from_slice(payload).map_err(|_| protocol())?;
    let Some(format) = envelope.pointer("/request/text/format") else {
        return Ok(None);
    };
    if format.get("type").and_then(Value::as_str) != Some("json_schema") {
        return Ok(None);
    }
    if format.as_object().is_none_or(|object| {
        object
            .keys()
            .any(|key| !matches!(key.as_str(), "type" | "name" | "strict" | "schema"))
    }) || format
        .get("strict")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(protocol());
    }
    let schema = crate::provider_response_schema::ResponseSchema::compile(
        format.get("schema").ok_or_else(protocol)?,
    )
    .map_err(|_| protocol())?;
    if !schema.is_object() {
        return Err(protocol());
    }
    Ok(Some(schema))
}

pub(crate) fn prepare_api_json_object_request(
    payload: &[u8],
    model: &str,
    max_output_tokens: u32,
) -> Result<Vec<u8>, HttpsSseProviderError> {
    prepare_api_local_schema_request(payload, model, max_output_tokens, false)
}

pub(crate) fn prepare_api_text_request(
    payload: &[u8],
    model: &str,
    max_output_tokens: u32,
) -> Result<Vec<u8>, HttpsSseProviderError> {
    prepare_api_local_schema_request(payload, model, max_output_tokens, true)
}

fn prepare_api_local_schema_request(
    payload: &[u8],
    model: &str,
    max_output_tokens: u32,
    omit_text: bool,
) -> Result<Vec<u8>, HttpsSseProviderError> {
    let schema = response_schema(payload)?;
    let bytes = prepare_api_request(payload, model, max_output_tokens)?;
    if schema.is_none() && !omit_text {
        return Ok(bytes);
    }
    let mut request: Value = serde_json::from_slice(&bytes).map_err(|_| protocol())?;
    if let Some(schema) = schema {
        let instructions = request
            .get("instructions")
            .and_then(Value::as_str)
            .ok_or_else(protocol)?;
        let schema_json = schema.json().map_err(|_| protocol())?;
        request["instructions"] = json!(format!(
            "{instructions}\n\nThe final assistant answer must be exactly one JSON object matching this JSON schema. Tool calls and intermediate commentary remain available. JSON schema:\n{schema_json}"
        ));
        if !omit_text {
            request["text"]["format"] = json!({"type":"json_object"});
        }
    }
    if omit_text {
        request.as_object_mut().ok_or_else(protocol)?.remove("text");
    }
    serde_json::to_vec(&request).map_err(|_| protocol())
}

fn prepare(
    payload: &[u8],
    model: &str,
    codex_backend: bool,
    max_output_tokens: Option<u32>,
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
    if let Some(limit) = max_output_tokens {
        request.insert("max_output_tokens".into(), json!(limit));
    }
    serde_json::to_vec(&request).map_err(|_| protocol())
}

pub(crate) fn parse_response(
    bytes: &[u8],
    receipt: &ProviderGatewayOpenReceipt,
    max_event_bytes: usize,
    max_events: usize,
) -> Result<HttpsSseProviderCompletion, HttpsSseProviderError> {
    parse_with_policy(
        bytes,
        receipt,
        max_event_bytes,
        max_events,
        ModelPolicy::Route,
        None,
    )
}

pub(crate) fn parse_api_response(
    bytes: &[u8],
    receipt: &ProviderGatewayOpenReceipt,
    max_event_bytes: usize,
    max_events: usize,
) -> Result<HttpsSseProviderCompletion, HttpsSseProviderError> {
    parse_with_policy(
        bytes,
        receipt,
        max_event_bytes,
        max_events,
        ModelPolicy::Observed,
        None,
    )
}

pub(crate) fn parse_api_response_with_schema(
    bytes: &[u8],
    receipt: &ProviderGatewayOpenReceipt,
    max_event_bytes: usize,
    max_events: usize,
    schema: Option<&crate::provider_response_schema::ResponseSchema>,
) -> Result<HttpsSseProviderCompletion, HttpsSseProviderError> {
    if schema.is_none() {
        return parse_api_response(bytes, receipt, max_event_bytes, max_events);
    }
    parse_with_policy(
        bytes,
        receipt,
        max_event_bytes,
        max_events,
        ModelPolicy::Observed,
        schema,
    )
}

fn parse_with_policy(
    bytes: &[u8],
    receipt: &ProviderGatewayOpenReceipt,
    max_event_bytes: usize,
    max_events: usize,
    model_policy: ModelPolicy,
    schema: Option<&crate::provider_response_schema::ResponseSchema>,
) -> Result<HttpsSseProviderCompletion, HttpsSseProviderError> {
    let mut stream = Responses::new(receipt, model_policy);
    stream.schema = schema;
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
    observed_receipt_with_policy(bytes, max_event_bytes, max_events, ModelPolicy::Route)
}

pub(crate) fn observed_api_receipt(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
) -> Option<(String, ProviderTokenUsage)> {
    observed_receipt_with_policy(bytes, max_event_bytes, max_events, ModelPolicy::Observed)
}

fn observed_receipt_with_policy(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
    model_policy: ModelPolicy,
) -> Option<(String, ProviderTokenUsage)> {
    let mut id = None;
    let mut usage = None;
    let mut model = None;
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
            if model_policy == ModelPolicy::Observed && response.get("model").is_some() {
                let observed = response_model(response).ok()?;
                if model
                    .as_deref()
                    .is_some_and(|previous| previous != observed)
                {
                    return None;
                }
                model = Some(observed.to_owned());
            }
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
                usage = Some(token_usage_for_policy(response, model_policy).ok()?);
            }
        }
    }
    Some((id?, usage?))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ModelPolicy {
    Route,
    Observed,
}

fn response_model(response: &Value) -> Result<&str, HttpsSseProviderError> {
    response
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| {
            !model.is_empty()
                && model.len() <= 256
                && model.trim() == *model
                && !model.chars().any(char::is_control)
        })
        .ok_or_else(protocol)
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
    model_policy: ModelPolicy,
    observed_model: Option<String>,
    schema: Option<&'a crate::provider_response_schema::ResponseSchema>,
    messages: Vec<Value>,
    has_tool_calls: bool,
}

impl<'a> Responses<'a> {
    fn new(receipt: &'a ProviderGatewayOpenReceipt, model_policy: ModelPolicy) -> Self {
        Self {
            receipt,
            frames: Vec::new(),
            response_id: None,
            items: BTreeSet::new(),
            done: BTreeSet::new(),
            terminal: None,
            latest_usage: None,
            output_observed: false,
            model_policy,
            observed_model: None,
            schema: None,
            messages: Vec::new(),
            has_tool_calls: false,
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
            return if self.model_policy == ModelPolicy::Route {
                self.emit(
                    &json!({"type":"server_model","model":self.receipt.route.model_id}),
                    false,
                )
            } else {
                self.observe_model(event)
            };
        }
        if matches!(kind, "response.failed" | "response.incomplete" | "error") {
            return self.fail(event);
        }
        if self.response_id.is_none() {
            return Err(protocol());
        }
        self.observe_model(event)?;
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
                if self.schema.is_some() && kind == "response.output_item.done" {
                    match item.get("type").and_then(Value::as_str) {
                        Some("function_call" | "custom_tool_call") => self.has_tool_calls = true,
                        Some("message") => self.messages.push(item.clone()),
                        _ => {}
                    }
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
                    || (self.model_policy == ModelPolicy::Observed && self.observed_model.is_none())
                {
                    return Err(protocol());
                }
                validate_completed_response(&event["response"], self.model_policy)?;
                let usage = token_usage_for_policy(&event["response"], self.model_policy)?;
                if !self.valid_final_output() {
                    self.emit(&json!({"type":"error","error":{"code":"RESPONSE_SCHEMA_INVALID","message":"Provider final output failed the requested JSON schema","retryable":false},"tokenUsage":{
                        "input_tokens":usage.input_tokens,"cached_input_tokens":usage.cached_input_tokens,"cache_write_input_tokens":usage.cache_write_input_tokens,
                        "output_tokens":usage.output_tokens,"reasoning_output_tokens":usage.reasoning_output_tokens,"total_tokens":usage.input_tokens+usage.output_tokens}}),true)?;
                    self.terminal = Some(ProviderGatewayTerminal::Failed {
                        failure: ModelAttemptFailureFact {
                            kind: crate::ModelAttemptFailureKind::Protocol,
                            certainty: ModelExecutionCertainty::OutputObserved,
                        },
                        charge: Some(ProviderGatewayTerminalCharge {
                            usage,
                            actual_cost_micros: None,
                        }),
                    });
                    return Ok(());
                }
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

    fn observe_model(&mut self, event: &Value) -> Result<(), HttpsSseProviderError> {
        if self.model_policy == ModelPolicy::Route {
            return Ok(());
        }
        let Some(response) = event
            .get("response")
            .filter(|response| response.get("model").is_some())
        else {
            return Ok(());
        };
        let model = response_model(response)?;
        if let Some(previous) = &self.observed_model {
            return if previous == model {
                Ok(())
            } else {
                Err(protocol())
            };
        }
        self.emit(&json!({"type":"server_model","model":model}), false)?;
        self.observed_model = Some(model.to_owned());
        Ok(())
    }

    fn valid_final_output(&self) -> bool {
        let Some(schema) = self.schema else {
            return true;
        };
        if self.messages.iter().any(|item| {
            item.get("role").and_then(Value::as_str) != Some("assistant")
                || item.get("phase").is_some_and(|phase| {
                    !matches!(phase.as_str(), Some("commentary" | "final_answer"))
                })
        }) {
            return false;
        }
        let mut finals = self.messages.iter().filter(|item| {
            item.get("phase").and_then(Value::as_str) == Some("final_answer")
                || (!self.has_tool_calls && item.get("phase").is_none())
        });
        let Some(final_item) = finals.next() else {
            return self.has_tool_calls;
        };
        if finals.next().is_some()
            || final_item.get("role").and_then(Value::as_str) != Some("assistant")
            || final_item
                .get("phase")
                .is_some_and(|phase| phase.as_str() != Some("final_answer"))
        {
            return false;
        }
        let Some(parts) = final_item
            .get("content")
            .and_then(Value::as_array)
            .filter(|parts| parts.len() == 1)
        else {
            return false;
        };
        parts[0].get("type").and_then(Value::as_str) == Some("output_text")
            && parts[0]
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| schema.accepts(text))
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
        self.latest_usage = Some(token_usage_for_policy(response, self.model_policy)?);
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
        self.observe_model(event)?;
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
                    observed_model_id: self.observed_model.clone(),
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

fn validate_completed_response(
    response: &Value,
    model_policy: ModelPolicy,
) -> Result<(), HttpsSseProviderError> {
    if model_policy == ModelPolicy::Observed
        && (response
            .get("status")
            .is_some_and(|status| status.as_str() != Some("completed"))
            || response.get("error").is_some_and(|error| !error.is_null()))
    {
        return Err(protocol());
    }
    Ok(())
}

fn token_usage_for_policy(
    response: &Value,
    model_policy: ModelPolicy,
) -> Result<ProviderTokenUsage, HttpsSseProviderError> {
    let usage = token_usage(response)?;
    if model_policy == ModelPolicy::Observed && response.pointer("/usage/total_tokens").is_some() {
        let expected_total = usage
            .input_tokens
            .checked_add(usage.output_tokens)
            .ok_or_else(protocol)?;
        if number(response, "/usage/total_tokens")? != expected_total {
            return Err(protocol());
        }
    }
    Ok(usage)
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

    fn api_custom_response_events() -> Vec<Value> {
        let source =
            "const result = await tools.exec_command({cmd:'printf ready'});\ntext(result);";
        vec![
            json!({"type":"response.created","response":{"id":"resp-custom","model":"observed-api-model"}}),
            json!({"type":"response.output_item.added","item":{"type":"custom_tool_call","id":"ct-one","call_id":"call-one","name":"exec","input":""}}),
            json!({"type":"response.custom_tool_call_input.delta","item_id":"ct-one","call_id":"call-one","delta":source}),
            json!({"type":"response.output_item.done","item":{"type":"custom_tool_call","id":"ct-one","call_id":"call-one","name":"exec","input":source}}),
            json!({"type":"response.completed","response":{"id":"resp-custom","model":"observed-api-model","usage":{"input_tokens":20,"output_tokens":7}}}),
        ]
    }

    fn structured_schema() -> crate::provider_response_schema::ResponseSchema {
        crate::provider_response_schema::ResponseSchema::compile(&json!({
            "type":"object","additionalProperties":false,"required":["verdict"],
            "properties":{"verdict":{"type":"string","enum":["pass","fail"]}}
        }))
        .unwrap()
    }

    fn structured_events(items: &[Value]) -> Vec<Value> {
        let mut events = vec![
            json!({"type":"response.created","response":{"id":"resp-one","model":"observed-api-model"}}),
        ];
        for item in items {
            events.push(json!({"type":"response.output_item.added","item":item}));
            events.push(json!({"type":"response.output_item.done","item":item}));
        }
        events.push(json!({"type":"response.completed","response":{"id":"resp-one","model":"observed-api-model","status":"completed","usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":1}},"end_turn":true}}));
        events
    }

    fn final_message(text: &str) -> Value {
        json!({"type":"message","id":"final-one","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":text}]})
    }

    fn unphased_tool_round_items(tool: Value) -> Vec<Value> {
        vec![
            json!({"type":"reasoning","id":"reasoning-one","summary":[]}),
            json!({"type":"message","id":"explanation-one","role":"assistant","content":[{"type":"output_text","text":"I will inspect the assigned workspace before reporting the verification result."}]}),
            tool,
        ]
    }

    fn unphased_native_tool() -> Value {
        json!({"type":"custom_tool_call","id":"tool-one","call_id":"call-one","name":"exec","input":"const result = await tools.exec_command({cmd:'npm run verify',workdir:'/public/workspace'});\ntext(result);"})
    }

    fn assert_local_schema_completed_with_exact_items(items: &[Value]) {
        let schema = structured_schema();
        let completion = parse_api_response_with_schema(
            &wire(&structured_events(items)),
            &receipt(),
            4096,
            100,
            Some(&schema),
        )
        .unwrap();
        let ProviderGatewayTerminal::Completed {
            usage,
            actual_cost_micros,
        } = completion.terminal
        else {
            panic!("a tool round requires model follow-up, not final JSON");
        };
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 4);
        assert_eq!(usage.cached_input_tokens, Some(2));
        assert_eq!(usage.reasoning_output_tokens, 1);
        assert_eq!(actual_cost_micros, None);
        let frames = completion
            .frames
            .iter()
            .map(|frame| serde_json::from_str::<Value>(frame.payload_json()).unwrap())
            .collect::<Vec<_>>();
        assert!(
            frames
                .iter()
                .any(|frame| frame["type"] == "server_model"
                    && frame["model"] == "observed-api-model")
        );
        let done = frames
            .iter()
            .filter(|frame| frame["type"] == "output_item_done")
            .map(|frame| frame["item"].clone())
            .collect::<Vec<_>>();
        assert_eq!(done, items, "native input and absent phases stay unchanged");
        assert_eq!(frames.last().unwrap()["type"], "completed");
        assert_eq!(frames.last().unwrap()["tokenUsage"]["total_tokens"], 14);
    }

    #[test]
    fn local_schema_responses_tool_round_preserves_unphased_text_and_native_input() {
        for tool in [
            unphased_native_tool(),
            json!({"type":"function_call","id":"tool-one","call_id":"call-one","name":"wait","arguments":"{\"cell_id\":\"one\"}"}),
        ] {
            let items = unphased_tool_round_items(tool);
            assert!(items[1].get("phase").is_none());
            assert_local_schema_completed_with_exact_items(&items);
        }
    }

    #[test]
    fn local_schema_responses_explicit_final_with_unphased_tool_explanation_is_validated() {
        let mut items = unphased_tool_round_items(unphased_native_tool());
        items.push(final_message(r#"{"verdict":"pass"}"#));
        assert_local_schema_completed_with_exact_items(&items);
    }

    fn assert_local_schema_rejected_with_paid_receipt(items: &[Value]) {
        let schema = structured_schema();
        let completion = parse_api_response_with_schema(
            &wire(&structured_events(items)),
            &receipt(),
            4096,
            100,
            Some(&schema),
        )
        .unwrap();
        let ProviderGatewayTerminal::Failed { charge, .. } = completion.terminal else {
            panic!("invalid final or explicit phase must remain rejected");
        };
        let usage = charge.unwrap().usage;
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 4);
        assert_eq!(usage.cached_input_tokens, Some(2));
        assert_eq!(usage.reasoning_output_tokens, 1);
        let frames = completion
            .frames
            .iter()
            .map(|frame| serde_json::from_str::<Value>(frame.payload_json()).unwrap())
            .collect::<Vec<_>>();
        assert!(
            frames
                .iter()
                .any(|frame| frame["type"] == "server_model"
                    && frame["model"] == "observed-api-model")
        );
        assert!(!frames.iter().any(|frame| frame["type"] == "completed"));
        let error = frames.last().unwrap();
        assert_eq!(error["error"]["code"], "RESPONSE_SCHEMA_INVALID");
        assert_eq!(error["error"]["retryable"], false);
        assert_eq!(error["tokenUsage"]["total_tokens"], 14);
    }

    #[test]
    fn local_schema_responses_tool_does_not_bypass_final_or_phase_validation() {
        let mut invalid_final = unphased_tool_round_items(unphased_native_tool());
        invalid_final.push(final_message(r#"{"verdict":"unknown"}"#));
        assert_local_schema_rejected_with_paid_receipt(&invalid_final);

        let mut multiple_finals = unphased_tool_round_items(unphased_native_tool());
        multiple_finals.push(final_message(r#"{"verdict":"pass"}"#));
        let mut second = final_message(r#"{"verdict":"pass"}"#);
        second["id"] = json!("final-two");
        multiple_finals.push(second);
        assert_local_schema_rejected_with_paid_receipt(&multiple_finals);

        let without_tool = unphased_tool_round_items(unphased_native_tool());
        assert_local_schema_rejected_with_paid_receipt(&without_tool[..2]);
        for role in [Some("user"), None] {
            let mut items = unphased_tool_round_items(unphased_native_tool());
            if let Some(role) = role {
                items[1]["role"] = json!(role);
            } else {
                items[1].as_object_mut().unwrap().remove("role");
            }
            assert_local_schema_rejected_with_paid_receipt(&items);
        }
        for phase in [
            json!("unknown"),
            Value::Null,
            json!(1),
            json!(false),
            json!([]),
            json!({}),
        ] {
            let mut items = unphased_tool_round_items(unphased_native_tool());
            items[1]["phase"] = phase;
            assert_local_schema_rejected_with_paid_receipt(&items);
        }
    }

    #[test]
    fn json_object_responses_accept_tool_only_and_tool_with_commentary() {
        let schema = structured_schema();
        for tool in [
            json!({"type":"custom_tool_call","id":"tool-one","call_id":"call-one","name":"exec","input":"text(true);"}),
            json!({"type":"function_call","id":"tool-one","call_id":"call-one","name":"wait","arguments":"{\"cell_id\":\"one\"}"}),
        ] {
            for commentary in [false, true] {
                let mut items = vec![tool.clone()];
                if commentary {
                    items.push(json!({"type":"message","id":"commentary-one","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Checking public fixture"}]}));
                }
                let completion = parse_api_response_with_schema(
                    &wire(&structured_events(&items)),
                    &receipt(),
                    4096,
                    100,
                    Some(&schema),
                )
                .unwrap();
                assert!(matches!(
                    completion.terminal,
                    ProviderGatewayTerminal::Completed { .. }
                ));
                let done = completion
                    .frames
                    .iter()
                    .map(|frame| serde_json::from_str::<Value>(frame.payload_json()).unwrap())
                    .filter(|frame| frame["type"] == "output_item_done")
                    .map(|frame| frame["item"].clone())
                    .collect::<Vec<_>>();
                assert_eq!(done, items);
            }
        }
    }

    #[test]
    fn json_object_responses_tool_with_invalid_explicit_final_is_rejected() {
        let schema = structured_schema();
        let items = [
            json!({"type":"custom_tool_call","id":"tool-one","call_id":"call-one","name":"exec","input":"text(true);"}),
            final_message(r#"{"verdict":"unknown"}"#),
        ];
        let completion = parse_api_response_with_schema(
            &wire(&structured_events(&items)),
            &receipt(),
            4096,
            100,
            Some(&schema),
        )
        .unwrap();
        assert!(
            matches!(completion.terminal, ProviderGatewayTerminal::Failed { charge: Some(charge), .. } if charge.usage.input_tokens == 10 && charge.usage.output_tokens == 4)
        );
    }

    #[test]
    fn json_object_responses_tool_with_multiple_explicit_finals_is_rejected() {
        let schema = structured_schema();
        let mut second = final_message(r#"{"verdict":"pass"}"#);
        second["id"] = json!("final-two");
        let items = [
            json!({"type":"function_call","id":"tool-one","call_id":"call-one","name":"wait","arguments":"{}"}),
            final_message(r#"{"verdict":"pass"}"#),
            second,
        ];
        let completion = parse_api_response_with_schema(
            &wire(&structured_events(&items)),
            &receipt(),
            4096,
            100,
            Some(&schema),
        )
        .unwrap();
        assert!(matches!(
            completion.terminal,
            ProviderGatewayTerminal::Failed {
                charge: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn json_object_responses_tool_with_valid_explicit_final_preserves_items() {
        let schema = structured_schema();
        let items = [
            json!({"type":"custom_tool_call","id":"tool-one","call_id":"call-one","name":"exec","input":"text(true);"}),
            final_message(r#"{"verdict":"pass"}"#),
        ];
        let completion = parse_api_response_with_schema(
            &wire(&structured_events(&items)),
            &receipt(),
            4096,
            100,
            Some(&schema),
        )
        .unwrap();
        assert!(matches!(
            completion.terminal,
            ProviderGatewayTerminal::Completed { .. }
        ));
        let done = completion
            .frames
            .iter()
            .map(|frame| serde_json::from_str::<Value>(frame.payload_json()).unwrap())
            .filter(|frame| frame["type"] == "output_item_done")
            .map(|frame| frame["item"].clone())
            .collect::<Vec<_>>();
        assert_eq!(done, items);
    }

    #[test]
    fn json_object_responses_final_validation_keeps_paid_receipt_and_real_model() {
        let schema = structured_schema();
        for text in [
            "{}",
            r#"{"verdict":"unknown"}"#,
            r#"{"verdict":true}"#,
            r#"{"verdict":"pass","extra":1}"#,
            r#"{"verdict":"fail","verdict":"pass"}"#,
            "```json\n{\"verdict\":\"pass\"}\n```",
            "{",
            r#"{"verdict":"pass"} trailing"#,
        ] {
            let completion = parse_api_response_with_schema(
                &wire(&structured_events(&[final_message(text)])),
                &receipt(),
                4096,
                100,
                Some(&schema),
            )
            .unwrap();
            assert!(
                matches!(completion.terminal, ProviderGatewayTerminal::Failed { failure: ModelAttemptFailureFact { kind: crate::ModelAttemptFailureKind::Protocol, certainty: ModelExecutionCertainty::OutputObserved }, charge: Some(charge) } if charge.usage.input_tokens == 10 && charge.usage.output_tokens == 4 && charge.usage.cached_input_tokens == Some(2) && charge.usage.reasoning_output_tokens == 1)
            );
            let frames = completion
                .frames
                .iter()
                .map(|frame| serde_json::from_str::<Value>(frame.payload_json()).unwrap())
                .collect::<Vec<_>>();
            assert!(
                frames.iter().any(|frame| frame["type"] == "server_model"
                    && frame["model"] == "observed-api-model")
            );
            assert!(!frames.iter().any(|frame| frame["type"] == "completed"));
            assert_eq!(
                frames.last().unwrap()["error"]["code"],
                "RESPONSE_SCHEMA_INVALID"
            );
            assert_eq!(frames.last().unwrap()["error"]["retryable"], false);
        }
        for phase in [Some("final_answer"), None] {
            let mut message = final_message(r#"{"verdict":"pass"}"#);
            if phase.is_none() {
                message.as_object_mut().unwrap().remove("phase");
            }
            let completion = parse_api_response_with_schema(
                &wire(&structured_events(&[message])),
                &receipt(),
                4096,
                100,
                Some(&schema),
            )
            .unwrap();
            assert!(matches!(
                completion.terminal,
                ProviderGatewayTerminal::Completed { .. }
            ));
        }
    }

    #[test]
    fn json_object_responses_reject_multiple_or_missing_final_without_joining_messages() {
        let schema = structured_schema();
        let first = final_message(r#"{"verdict":"pass"}"#);
        let mut second = first.clone();
        second["id"] = json!("final-two");
        let mut commentary = first.clone();
        commentary["phase"] = json!("commentary");
        let mut unknown_phase = first.clone();
        unknown_phase["phase"] = json!("unknown");
        let mut content_parts = first;
        content_parts["content"] = json!([{"type":"output_text","text":"{\"verdict\":"},{"type":"output_text","text":"\"pass\"}"}]);
        for items in [
            vec![final_message("{\"verdict\":"), second],
            vec![commentary],
            vec![unknown_phase],
            vec![content_parts],
            vec![json!({"type":"reasoning","id":"reasoning-one","summary":[]})],
        ] {
            let completion = parse_api_response_with_schema(
                &wire(&structured_events(&items)),
                &receipt(),
                4096,
                100,
                Some(&schema),
            )
            .unwrap();
            assert!(matches!(
                completion.terminal,
                ProviderGatewayTerminal::Failed {
                    charge: Some(_),
                    ..
                }
            ));
        }
    }

    #[test]
    fn json_object_responses_wire_is_deterministic_and_preserves_default_and_native_tools() {
        let envelope = json!({"request":{"model":"test-model","instructions":"Public fixture","input":[{"type":"reasoning","id":"reasoning-one","encrypted_content":"public-encrypted-fixture"},{"type":"custom_tool_call","id":"tool-one","call_id":"call-one","name":"exec","input":"text(true);"},{"type":"custom_tool_call_output","call_id":"call-one","output":"true"}],"tools":[{"type":"custom","name":"exec","format":{"type":"grammar","syntax":"lark","definition":"start: /[\\s\\S]+/"}},{"type":"function","name":"wait","parameters":{"type":"object"}}],"text":{"format":{"type":"json_schema","name":"public_result","strict":true,"schema":{"type":"object","additionalProperties":false,"required":["verdict"],"properties":{"verdict":{"type":"string","enum":["pass","fail"]}}}}}}});
        let payload = serde_json::to_vec(&envelope).unwrap();
        let compatible = prepare_api_json_object_request(&payload, "test-model", 32_768).unwrap();
        assert_eq!(
            compatible,
            prepare_api_json_object_request(&payload, "test-model", 32_768).unwrap()
        );
        let wire: Value = serde_json::from_slice(&compatible).unwrap();
        assert_eq!(wire["tools"], envelope["request"]["tools"]);
        assert_eq!(wire["input"], envelope["request"]["input"]);
        assert_eq!(wire["text"]["format"], json!({"type":"json_object"}));
        let schema =
            serde_json::to_string(&envelope["request"]["text"]["format"]["schema"]).unwrap();
        assert_eq!(
            wire["instructions"]
                .as_str()
                .unwrap()
                .matches(&schema)
                .count(),
            1
        );
        let native: Value =
            serde_json::from_slice(&prepare_api_request(&payload, "test-model", 32_768).unwrap())
                .unwrap();
        assert_eq!(native["text"], envelope["request"]["text"]);
        assert_eq!(native["instructions"], envelope["request"]["instructions"]);
        let subscription: Value =
            serde_json::from_slice(&prepare_request(&payload, "test-model").unwrap()).unwrap();
        assert_eq!(subscription["text"], envelope["request"]["text"]);
        let mut executor = envelope.clone();
        executor["request"].as_object_mut().unwrap().remove("text");
        let executor = serde_json::to_vec(&executor).unwrap();
        assert_eq!(
            prepare_api_json_object_request(&executor, "test-model", 32_768).unwrap(),
            prepare_api_request(&executor, "test-model", 32_768).unwrap()
        );
        assert_eq!(serde_json::from_slice::<Value>(&payload).unwrap(), envelope);
    }

    #[test]
    fn text_responses_wire_keeps_legacy_json_object_and_native_schema_contracts() {
        let schema = json!({"type":"object","required":["verdict"],"properties":{
            "verdict":{"type":"string","enum":["pass","fail"]}
        }});
        let tools = json!([{"type":"custom","name":"exec","format":{
            "type":"grammar","syntax":"lark","definition":"start: /[\\s\\S]+/"
        }}]);
        let history = json!([
            {"type":"custom_tool_call","name":"exec","call_id":"old-call","input":"text(true);"},
            {"type":"custom_tool_call_output","call_id":"old-call","output":"true"}
        ]);
        let envelope = json!({"request":{"model":"test-model","instructions":"Review fixture",
            "input":history,"tools":tools,"text":{"format":{"type":"json_schema","schema":schema}}
        }});
        let payload = serde_json::to_vec(&envelope).unwrap();
        let expected_instructions = format!(
            "Review fixture\n\nThe final assistant answer must be exactly one JSON object matching this JSON schema. Tool calls and intermediate commentary remain available. JSON schema:\n{}",
            serde_json::to_string(&schema).unwrap()
        );
        let expected_json_object = json!({"model":"test-model","instructions":expected_instructions,
            "input":history,"tools":tools,"text":{"format":{"type":"json_object"}},
            "stream":true,"store":false,"max_output_tokens":32_768
        });
        assert_eq!(
            prepare_api_json_object_request(&payload, "test-model", 32_768).unwrap(),
            serde_json::to_vec(&expected_json_object).unwrap()
        );
        let mut expected_text = expected_json_object;
        expected_text.as_object_mut().unwrap().remove("text");
        let text = prepare_api_text_request(&payload, "test-model", 32_768).unwrap();
        assert_eq!(text, serde_json::to_vec(&expected_text).unwrap());
        assert_eq!(
            text,
            prepare_api_text_request(&payload, "test-model", 32_768).unwrap()
        );
        let native: Value =
            serde_json::from_slice(&prepare_api_request(&payload, "test-model", 32_768).unwrap())
                .unwrap();
        assert_eq!(native["text"], envelope["request"]["text"]);
        assert_eq!(native["instructions"], "Review fixture");
        let mut executor = envelope.clone();
        executor["request"].as_object_mut().unwrap().remove("text");
        let executor = serde_json::to_vec(&executor).unwrap();
        assert_eq!(
            prepare_api_text_request(&executor, "test-model", 32_768).unwrap(),
            prepare_api_request(&executor, "test-model", 32_768).unwrap()
        );
        assert_eq!(serde_json::from_slice::<Value>(&payload).unwrap(), envelope);
    }

    #[test]
    fn text_responses_omits_the_whole_text_container_without_a_schema() {
        let payload = serde_json::to_vec(&json!({"request":{
            "model":"test-model","input":[],"text":{"format":{"type":"text"},"verbosity":"low"}
        }}))
        .unwrap();
        let wire: Value = serde_json::from_slice(
            &prepare_api_text_request(&payload, "test-model", 32_768).unwrap(),
        )
        .unwrap();
        assert!(wire.get("text").is_none());
        assert_eq!(wire["instructions"], "");
        assert!(response_schema(&payload).unwrap().is_none());
    }

    #[test]
    fn json_object_responses_reject_schema_private_marker_object_enum_before_projection() {
        for marker in [
            "$serde_json::private::Number",
            r"\u0024serde_json::private::Number",
        ] {
            let payload = format!(
                r#"{{"request":{{"model":"test-model","input":[],"text":{{"format":{{"type":"json_schema","schema":{{"type":"object","properties":{{"schemaVersion":{{"type":"integer","enum":[{{"{marker}":"1"}}]}}}}}}}}}}}}}}"#
            );
            assert!(response_schema(payload.as_bytes()).is_err());
            assert!(
                prepare_api_json_object_request(payload.as_bytes(), "test-model", 32_768).is_err()
            );
            assert!(prepare_api_text_request(payload.as_bytes(), "test-model", 32_768).is_err());
        }
    }

    #[test]
    fn api_responses_observes_upstream_model_and_preserves_native_custom_input() {
        let events = api_custom_response_events();
        let completion = parse_api_response(&wire(&events), &receipt(), 4096, 100).unwrap();
        let frames = completion
            .frames
            .iter()
            .map(|frame| serde_json::from_str::<Value>(frame.payload_json()).unwrap())
            .collect::<Vec<_>>();
        let observed = frames
            .iter()
            .find(|frame| frame["type"] == "server_model")
            .unwrap();
        assert_eq!(observed["model"], "observed-api-model");
        assert_ne!(observed["model"], receipt().route.model_id);
        let delta = frames
            .iter()
            .find(|frame| frame["type"] == "tool_call_input_delta")
            .unwrap();
        assert_eq!(delta["delta"], events[2]["delta"]);
        let done = frames
            .iter()
            .find(|frame| frame["type"] == "output_item_done")
            .unwrap();
        assert_eq!(done["item"], events[3]["item"]);
        assert_eq!(done["item"]["type"], "custom_tool_call");
        assert!(completion.frames.last().unwrap().is_terminal());
    }

    #[test]
    fn api_responses_rejects_model_drift_between_created_and_completed() {
        let mut events = api_custom_response_events();
        events.last_mut().unwrap()["response"]["model"] = json!("different-api-model");
        assert!(parse_api_response(&wire(&events), &receipt(), 4096, 100).is_err());
    }

    #[test]
    fn api_responses_does_not_substitute_the_route_for_missing_upstream_model() {
        let mut events = api_custom_response_events();
        for event in &mut events {
            if let Some(response) = event.get_mut("response").and_then(Value::as_object_mut) {
                response.remove("model");
            }
        }
        assert!(parse_api_response(&wire(&events), &receipt(), 4096, 100).is_err());
    }

    #[test]
    fn api_responses_accepts_model_observed_only_at_completion() {
        let mut events = api_custom_response_events();
        events[0]["response"]
            .as_object_mut()
            .unwrap()
            .remove("model");
        let completion = parse_api_response(&wire(&events), &receipt(), 4096, 100).unwrap();
        let models = completion
            .frames
            .iter()
            .filter_map(|frame| {
                let value: Value = serde_json::from_str(frame.payload_json()).unwrap();
                (value["type"] == "server_model").then(|| value["model"].clone())
            })
            .collect::<Vec<_>>();
        assert_eq!(models, [json!("observed-api-model")]);
    }

    #[test]
    fn api_responses_preparation_keeps_native_tools_and_history_with_bounded_output() {
        let source = "text('quote: \\\"; slash: \\\\; 中文');\ntext(true);";
        let tools = json!([{"type":"custom","name":"exec","description":"raw source, not JSON","format":{"type":"grammar","syntax":"lark","definition":"start: /[\\s\\S]+/"}}]);
        let history = json!([
            {"type":"custom_tool_call","name":"exec","call_id":"call-before","input":source},
            {"type":"custom_tool_call_output","call_id":"call-before","output":"completed"}
        ]);
        let payload = serde_json::to_vec(&json!({"request":{"model":"test-model","instructions":"Use the tool","input":history,"tools":tools,"max_output_tokens":100_000,"stream":true,"store":false}})).unwrap();
        let prepared: Value =
            serde_json::from_slice(&prepare_api_request(&payload, "test-model", 32_768).unwrap())
                .unwrap();
        assert_eq!(prepared["tools"], tools);
        assert_eq!(prepared["input"], history);
        assert_eq!(prepared["max_output_tokens"], 32_768);
        assert!(prepared["tools"][0].get("input_schema").is_none());
        assert!(prepare_api_request(&payload, "other-model", 32_768).is_err());
        assert!(prepare_api_request(&payload, "test-model", 0).is_err());
    }

    #[test]
    fn api_responses_partial_usage_is_bound_to_observed_model() {
        let mut events = api_custom_response_events();
        events.last_mut().unwrap()["type"] = json!("response.failed");
        events.last_mut().unwrap()["response"]["error"] = json!({"code":"server_error"});
        let completion = parse_api_response(&wire(&events), &receipt(), 4096, 100).unwrap();
        let ProviderGatewayTerminal::Failed {
            charge: Some(charge),
            ..
        } = completion.terminal
        else {
            panic!("failed Responses call must retain observed paid usage");
        };
        assert_eq!(charge.usage.input_tokens, 20);
        assert_eq!(charge.usage.output_tokens, 7);
        assert_eq!(charge.actual_cost_micros, None);
        assert_eq!(
            observed_api_receipt(&wire(&events), 4096, 100).unwrap().0,
            "resp-custom"
        );
        events.last_mut().unwrap()["response"]["model"] = json!("different-api-model");
        assert!(parse_api_response(&wire(&events), &receipt(), 4096, 100).is_err());
        assert!(observed_api_receipt(&wire(&events), 4096, 100).is_none());
    }

    #[test]
    fn api_responses_rejects_completed_event_with_failed_status() {
        let mut events = api_custom_response_events();
        events.last_mut().unwrap()["response"]["status"] = json!("failed");
        assert!(parse_api_response(&wire(&events), &receipt(), 4096, 100).is_err());
    }

    #[test]
    fn api_responses_rejects_completed_event_with_incomplete_status() {
        let mut events = api_custom_response_events();
        events.last_mut().unwrap()["response"]["status"] = json!("incomplete");
        assert!(parse_api_response(&wire(&events), &receipt(), 4096, 100).is_err());
    }

    #[test]
    fn api_responses_rejects_completed_event_with_nonnull_error() {
        let mut events = api_custom_response_events();
        events.last_mut().unwrap()["response"]["error"] =
            json!({"code":"server_error","message":"execution failed"});
        assert!(parse_api_response(&wire(&events), &receipt(), 4096, 100).is_err());
    }

    #[test]
    fn api_responses_rejects_mismatched_reported_total_tokens() {
        let mut events = api_custom_response_events();
        events.last_mut().unwrap()["response"]["usage"]["total_tokens"] = json!(999);
        assert!(parse_api_response(&wire(&events), &receipt(), 4096, 100).is_err());
        assert!(observed_api_receipt(&wire(&events), 4096, 100).is_none());
    }

    #[test]
    fn api_responses_preserves_known_usage_after_invalid_completed_status() {
        let mut events = api_custom_response_events();
        let response = &mut events.last_mut().unwrap()["response"];
        response["status"] = json!("failed");
        response["usage"]["total_tokens"] = json!(27);
        assert!(parse_api_response(&wire(&events), &receipt(), 4096, 100).is_err());
        let (_, usage) = observed_api_receipt(&wire(&events), 4096, 100).unwrap();
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.output_tokens, 7);
    }

    #[test]
    fn api_responses_preserves_paid_failure_with_consistent_explicit_total() {
        let mut events = api_custom_response_events();
        let terminal = events.last_mut().unwrap();
        terminal["type"] = json!("response.failed");
        terminal["response"]["status"] = json!("failed");
        terminal["response"]["error"] = json!({"code":"server_error"});
        terminal["response"]["usage"]["total_tokens"] = json!(27);
        let completion = parse_api_response(&wire(&events), &receipt(), 4096, 100).unwrap();
        let ProviderGatewayTerminal::Failed {
            charge: Some(charge),
            ..
        } = completion.terminal
        else {
            panic!("known usage must survive a paid provider failure");
        };
        assert_eq!(charge.usage.input_tokens, 20);
        assert_eq!(charge.usage.output_tokens, 7);
        assert_eq!(charge.actual_cost_micros, None);
        assert!(observed_api_receipt(&wire(&events), 4096, 100).is_some());
    }

    #[test]
    fn api_responses_accepts_consistent_explicit_completed_fields() {
        let mut events = api_custom_response_events();
        let response = &mut events.last_mut().unwrap()["response"];
        response["status"] = json!("completed");
        response["error"] = Value::Null;
        response["usage"]["total_tokens"] = json!(27);
        let completion = parse_api_response(&wire(&events), &receipt(), 4096, 100).unwrap();
        assert!(matches!(
            completion.terminal,
            ProviderGatewayTerminal::Completed { .. }
        ));
    }

    #[test]
    fn api_responses_accepts_absent_optional_completed_fields() {
        let events = api_custom_response_events();
        let response = &events.last().unwrap()["response"];
        assert!(response.get("status").is_none());
        assert!(response.get("error").is_none());
        assert!(response["usage"].get("total_tokens").is_none());
        assert!(parse_api_response(&wire(&events), &receipt(), 4096, 100).is_ok());
    }

    #[test]
    fn subscription_responses_keeps_its_existing_route_model_policy() {
        let completion = parse_response(&wire(&response_events()), &receipt(), 4096, 100).unwrap();
        let model: Value = serde_json::from_str(completion.frames[1].payload_json()).unwrap();
        assert_eq!(model["model"], receipt().route.model_id);
    }
}
