// SPDX-License-Identifier: Apache-2.0

//! `OpenAI` chat/completions protocol translation for the external HTTPS/SSE adapter.
//!
//! Converts the Codex `ModelStreamRequest` JSON shape into an `OpenAI`
//! `chat.completions` body and converts `chat.completion.chunk` SSE frames into
//! canonical `ProviderStreamEvent`s. The configured endpoint is used verbatim;
//! this module never forces a `/v1/messages` suffix.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::provider_anthropic::{
    AnthropicCodecError, AnthropicMessagesOptions, AnthropicToolBindings, PreparedAnthropicRequest,
    ProviderTokenPricing, array, exact_keys, object, optional_string, prepare_anthropic_request,
    required, string, validate_text, validate_token,
};
use crate::{
    ProviderFinishReason, ProviderGatewayTerminal, ProviderStreamEvent, ProviderTokenUsage,
};

const MAX_REQUEST_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Translates one Codex `ModelStreamRequest` payload into an `OpenAI` chat body.
///
/// # Errors
///
/// Rejects malformed envelopes, unsafe token limits, and size overflows.
pub(crate) fn prepare_openai_chat_request(
    payload: &[u8],
    upstream_model_id: &str,
    options: AnthropicMessagesOptions,
) -> Result<PreparedAnthropicRequest, AnthropicCodecError> {
    let prepared = prepare_anthropic_request(payload, upstream_model_id, options)?;
    let normalized: Value = serde_json::from_slice(&prepared.body)
        .map_err(|_| AnthropicCodecError::invalid_request())?;
    let normalized = object(&normalized)?;
    let request: Value =
        serde_json::from_slice(payload).map_err(|_| AnthropicCodecError::invalid_request())?;
    let mut body = json!({
        "model": upstream_model_id, "max_tokens": options.max_output_tokens,
        "stream": true, "stream_options": {"include_usage": true},
        "parallel_tool_calls": request["request"]["parallel_tool_calls"],
        "messages": openai_messages(optional_string(normalized, "system")?.unwrap_or_default(), array(normalized, "messages")?)?,
    });
    if let Some(effort) = request
        .pointer("/request/reasoning/effort")
        .filter(|value| !value.is_null())
    {
        body["reasoning_effort"] = effort.clone();
    }
    if let Some(format) = request.pointer("/request/text/format") {
        body["response_format"] = json!({"type": "json_schema", "json_schema": {
            "name": format["name"], "strict": format["strict"], "schema": format["schema"]
        }});
    }
    if let Some(tools) = normalized.get("tools") {
        body["tools"] = Value::Array(
            tools
                .as_array()
                .ok_or_else(AnthropicCodecError::invalid_request)?
                .iter()
                .map(|tool| {
                    let tool = object(tool)?;
                    Ok(json!({"type":"function","function": {
                        "name": required(tool, "name")?,
                        "description": tool.get("description").cloned().unwrap_or_default(),
                        "parameters": required(tool, "input_schema")?,
                    }}))
                })
                .collect::<Result<Vec<_>, AnthropicCodecError>>()?,
        );
        body["tool_choice"] = request["request"]["tool_choice"].clone();
    }
    let body = serde_json::to_vec(&body).map_err(|_| AnthropicCodecError::invalid_request())?;
    if body.len() > MAX_REQUEST_TEXT_BYTES {
        return Err(AnthropicCodecError::size_limit());
    }
    Ok(PreparedAnthropicRequest {
        body,
        tool_bindings: prepared.tool_bindings,
    })
}

fn openai_messages(
    instructions: &str,
    anthropic_messages: &[Value],
) -> Result<Vec<Value>, AnthropicCodecError> {
    let mut messages = Vec::new();
    if !instructions.is_empty() {
        messages.push(json!({"role":"system","content":instructions}));
    }
    for message in anthropic_messages {
        let message = object(message)?;
        let role = string(message, "role")?;
        let blocks = array(message, "content")?;
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut tool_results: Vec<Value> = Vec::new();
        let mut images: Vec<Value> = Vec::new();
        for block in blocks {
            let block = object(block)?;
            match string(block, "type")? {
                "text" => {
                    let text = string(block, "text")?;
                    validate_text(text)?;
                    if !text.is_empty() {
                        text_parts.push(text.to_owned());
                    }
                }
                "tool_use" => {
                    let id = string(block, "id")?;
                    let name = string(block, "name")?;
                    let input = required(block, "input")?;
                    let arguments = serde_json::to_string(input)
                        .map_err(|_| AnthropicCodecError::invalid_request())?;
                    validate_text(&arguments)?;
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }));
                }
                "tool_result" => {
                    let id = string(block, "tool_use_id")?;
                    let content = required(block, "content")?;
                    let text = flatten_tool_result_text(content)?;
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": text,
                    }));
                }
                "image" => {
                    let source = object(required(block, "source")?)?;
                    exact_keys(source, &["type", "media_type", "data"])?;
                    let media_type = string(source, "media_type")?;
                    let data = string(source, "data")?;
                    images.push(json!({
                        "type": "image_url",
                        "image_url": {"url": format!("data:{media_type};base64,{data}")},
                    }));
                }
                _ => return Err(AnthropicCodecError::invalid_request()),
            }
        }
        if role == "user" {
            for result in tool_results {
                messages.push(result);
            }
            if !text_parts.is_empty() || !images.is_empty() {
                let mut content: Vec<Value> = Vec::new();
                if !text_parts.is_empty() {
                    if images.is_empty() && text_parts.len() == 1 {
                        messages.push(json!({"role": "user", "content": text_parts[0]}));
                        continue;
                    }
                    content.push(json!({"type": "text", "text": text_parts.join("\n")}));
                }
                content.extend(images);
                messages.push(json!({"role": "user", "content": content}));
            }
        } else if role == "assistant" {
            let mut assistant = Map::new();
            assistant.insert("role".to_owned(), json!("assistant"));
            if text_parts.is_empty() {
                assistant.insert("content".to_owned(), Value::Null);
            } else {
                assistant.insert("content".to_owned(), json!(text_parts.join("\n")));
            }
            if !tool_calls.is_empty() {
                assistant.insert("tool_calls".to_owned(), Value::Array(tool_calls));
            }
            if assistant.get("content").is_some_and(Value::is_null)
                && !assistant.contains_key("tool_calls")
            {
                return Err(AnthropicCodecError::invalid_request());
            }
            messages.push(Value::Object(assistant));
        } else {
            return Err(AnthropicCodecError::invalid_request());
        }
    }
    Ok(messages)
}

fn flatten_tool_result_text(content: &Value) -> Result<String, AnthropicCodecError> {
    if let Some(text) = content.as_str() {
        validate_text(text)?;
        return Ok(text.to_owned());
    }
    let blocks = content
        .as_array()
        .ok_or_else(AnthropicCodecError::invalid_request)?;
    let mut parts = Vec::new();
    for block in blocks {
        let block = object(block)?;
        exact_keys(block, &["type", "text"])?;
        if string(block, "type")? != "text" {
            return Err(AnthropicCodecError::invalid_request());
        }
        let text = string(block, "text")?;
        validate_text(text)?;
        if !text.is_empty() {
            parts.push(text.to_owned());
        }
    }
    Ok(parts.join("\n"))
}

/// Parsed `OpenAI` chat stream: canonical events plus a terminal fact.
pub(crate) struct ParsedOpenAiStream {
    pub events: Vec<ProviderStreamEvent>,
    pub terminal: ProviderGatewayTerminal,
}

/// Converts `chat.completion.chunk` SSE into canonical Provider stream events.
///
/// # Errors
///
/// Rejects malformed chunks, lifecycle violations, and size overflows.
pub(crate) fn parse_openai_chat_sse(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
    tool_bindings: &AnthropicToolBindings,
    options: AnthropicMessagesOptions,
) -> Result<ParsedOpenAiStream, AnthropicCodecError> {
    options.validate()?;
    let wire =
        parse_openai_sse_envelopes(bytes, max_event_bytes, max_events).inspect_err(|error| {
            eprintln!("openai_sse_protocol stage=envelope kind={:?}", error.kind());
        })?;
    let mut parser = OpenAiStreamParser::new(tool_bindings, options.pricing, wire.len());
    for (index, envelope) in wire.iter().enumerate() {
        parser.push(envelope).inspect_err(|error| {
            let data = envelope.data.as_object();
            let choices = data.and_then(|value| value.get("choices")).and_then(Value::as_array);
            let delta = choices.and_then(|value| value.first())
                .and_then(|value| value.get("delta")).and_then(Value::as_object);
            eprintln!(
                "openai_sse_protocol stage=chunk index={index} kind={:?} choices={} usage={} finish={} tool_calls={} reasoning={} content={}",
                error.kind(), choices.map_or(0, Vec::len),
                data.is_some_and(|value| value.get("usage").is_some_and(|value| !value.is_null())),
                choices.is_some_and(|value| value.iter().any(|choice| choice.get("finish_reason")
                    .is_some_and(|value| !value.is_null()))),
                delta.is_some_and(|value| value.contains_key("tool_calls")),
                delta.is_some_and(|value| value.contains_key("reasoning_content") || value.contains_key("reasoning")),
                delta.is_some_and(|value| value.contains_key("content")),
            );
        })?;
    }
    let finished = parser.finish_reason.is_some();
    let measured = parser.usage.is_some();
    let open_tools = parser.open_tools.len();
    parser.finish().inspect_err(|error| {
        eprintln!("openai_sse_protocol stage=finish kind={:?} finish_seen={finished} usage_seen={measured} open_tools={open_tools}", error.kind());
    })
}

struct OpenAiEnvelope {
    data: Value,
}

fn parse_openai_sse_envelopes(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
) -> Result<Vec<OpenAiEnvelope>, AnthropicCodecError> {
    let frames =
        crate::provider_sse_framing::parse(bytes, max_event_bytes, max_events.saturating_add(1))
            .map_err(|error| match error {
                crate::provider_sse_framing::SseFramingError::Utf8 => {
                    AnthropicCodecError::invalid_sse()
                }
                crate::provider_sse_framing::SseFramingError::SizeLimit => {
                    AnthropicCodecError::size_limit()
                }
            })?;
    let mut envelopes = Vec::new();
    let mut done = false;
    for frame in frames {
        let mut data = frame.data;
        flush_openai_envelope(
            &mut envelopes,
            &mut data,
            &mut done,
            max_event_bytes,
            max_events,
        )?;
    }
    if !done {
        return Err(AnthropicCodecError::incomplete_stream());
    }
    Ok(envelopes)
}

fn flush_openai_envelope(
    envelopes: &mut Vec<OpenAiEnvelope>,
    data: &mut String,
    done: &mut bool,
    max_event_bytes: usize,
    max_events: usize,
) -> Result<(), AnthropicCodecError> {
    if data.is_empty() {
        return Ok(());
    }
    if data == "[DONE]" {
        if *done {
            return Err(AnthropicCodecError::protocol());
        }
        data.clear();
        *done = true;
        return Ok(());
    }
    if *done {
        return Err(AnthropicCodecError::protocol());
    }
    if data.len() > max_event_bytes || envelopes.len() >= max_events {
        return Err(AnthropicCodecError::size_limit());
    }
    let value: Value = serde_json::from_str(data).map_err(|_| AnthropicCodecError::protocol())?;
    envelopes.push(OpenAiEnvelope { data: value });
    data.clear();
    Ok(())
}

struct OpenAiStreamParser<'a> {
    tool_bindings: &'a AnthropicToolBindings,
    pricing: ProviderTokenPricing,
    events: Vec<ProviderStreamEvent>,
    open_tools: BTreeMap<u32, String>,
    custom_arguments: BTreeMap<u32, String>,
    text_open: bool,
    reasoning_open: bool,
    started: bool,
    usage: Option<ProviderTokenUsage>,
    finish_reason: Option<ProviderFinishReason>,
    provider_response_id: Option<String>,
}

impl<'a> OpenAiStreamParser<'a> {
    fn new(
        tool_bindings: &'a AnthropicToolBindings,
        pricing: ProviderTokenPricing,
        capacity: usize,
    ) -> Self {
        Self {
            tool_bindings,
            pricing,
            events: Vec::with_capacity(capacity.saturating_add(4)),
            open_tools: BTreeMap::new(),
            custom_arguments: BTreeMap::new(),
            text_open: false,
            reasoning_open: false,
            started: false,
            usage: None,
            finish_reason: None,
            provider_response_id: None,
        }
    }

    fn push(&mut self, envelope: &OpenAiEnvelope) -> Result<(), AnthropicCodecError> {
        let value = object(&envelope.data)?;
        if let Some(kind) = optional_string(value, "object")?
            && kind != "chat.completion.chunk"
        {
            return Err(AnthropicCodecError::protocol());
        }
        if let Some(id) = optional_string(value, "id")? {
            validate_token(id, 256)?;
            if !self.started {
                self.started = true;
                self.provider_response_id = Some(id.to_owned());
                self.events.push(ProviderStreamEvent::ResponseStarted {
                    provider_response_id: id.to_owned(),
                    observed_model_id: optional_string(value, "model")?.map(str::to_owned),
                });
            } else if self.provider_response_id.as_deref() != Some(id) {
                return Err(AnthropicCodecError::protocol());
            }
        } else if !self.started {
            return Err(AnthropicCodecError::protocol());
        }

        if let Some(usage) = value.get("usage")
            && !usage.is_null()
        {
            self.usage = Some(openai_usage(usage)?);
        }

        let Some(choices) = value.get("choices") else {
            return Ok(());
        };
        if choices.is_null() {
            return Ok(());
        }
        let choices = choices
            .as_array()
            .ok_or_else(AnthropicCodecError::protocol)?;
        for choice in choices {
            let choice = object(choice)?;
            exact_keys(choice, &["index", "delta", "finish_reason", "logprobs"])?;
            let index = choice
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(AnthropicCodecError::protocol)?;
            if index != 0 || self.finish_reason.is_some() {
                return Err(AnthropicCodecError::protocol());
            }
            if let Some(delta) = choice.get("delta")
                && !delta.is_null()
            {
                self.apply_delta(object(delta)?)?;
            }
            if let Some(finish) = choice.get("finish_reason")
                && !finish.is_null()
            {
                if self.finish_reason.is_some() {
                    return Err(AnthropicCodecError::protocol());
                }
                let finish = finish.as_str().ok_or_else(AnthropicCodecError::protocol)?;
                self.finish_reason = Some(match finish {
                    "stop" => ProviderFinishReason::Stop,
                    "tool_calls" | "function_call" => ProviderFinishReason::ToolCalls,
                    "length" => ProviderFinishReason::MaxTokens,
                    "content_filter" => ProviderFinishReason::Filtered,
                    "aborted" | "insufficient_system_resource" => ProviderFinishReason::Interrupted,
                    _ => return Err(AnthropicCodecError::protocol()),
                });
            }
        }
        Ok(())
    }

    fn apply_delta(&mut self, delta: &Map<String, Value>) -> Result<(), AnthropicCodecError> {
        if let Some(role) = delta.get("role")
            && !role.is_null()
            && role.as_str() != Some("assistant")
        {
            return Err(AnthropicCodecError::protocol());
        }
        if let Some(reasoning) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            && !reasoning.is_null()
        {
            let text = reasoning
                .as_str()
                .ok_or_else(AnthropicCodecError::invalid_request)?;
            validate_text(text)?;
            if !text.is_empty() {
                if !self.reasoning_open {
                    self.reasoning_open = true;
                    self.events.push(ProviderStreamEvent::ReasoningStarted {
                        index: 0,
                        summary_index: 0,
                    });
                }
                self.events
                    .push(ProviderStreamEvent::ReasoningContentDelta {
                        index: 0,
                        content_index: 0,
                        delta: text.to_owned(),
                    });
            }
        }
        if let Some(content) = delta.get("content")
            && !content.is_null()
            && content.as_str() != Some("")
        {
            let text = content
                .as_str()
                .ok_or_else(AnthropicCodecError::invalid_request)?;
            validate_text(text)?;
            self.close_reasoning();
            if !self.text_open {
                self.text_open = true;
                self.events
                    .push(ProviderStreamEvent::TextStarted { index: 1 });
            }
            if !text.is_empty() {
                self.events.push(ProviderStreamEvent::TextDelta {
                    index: 1,
                    delta: text.to_owned(),
                });
            }
        }
        if let Some(tool_calls) = delta.get("tool_calls")
            && !tool_calls.is_null()
        {
            self.close_reasoning();
            if self.text_open {
                self.text_open = false;
                self.events
                    .push(ProviderStreamEvent::TextEnded { index: 1 });
            }
            let tool_calls = tool_calls
                .as_array()
                .ok_or_else(AnthropicCodecError::protocol)?;
            for call in tool_calls {
                self.apply_tool_call(call)?;
            }
        }
        Ok(())
    }

    fn close_reasoning(&mut self) {
        if self.reasoning_open {
            self.reasoning_open = false;
            self.events
                .push(ProviderStreamEvent::ReasoningEnded { index: 0 });
        }
    }

    fn apply_tool_call(&mut self, call: &Value) -> Result<(), AnthropicCodecError> {
        let call = object(call)?;
        exact_keys(call, &["index", "id", "type", "function"])?;
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(AnthropicCodecError::protocol)?;
        let function = object(required(call, "function")?)?;
        exact_keys(function, &["name", "arguments"])?;
        let index = index
            .checked_add(2)
            .ok_or_else(AnthropicCodecError::protocol)?;
        if let Some(existing_id) = self.open_tools.get(&index) {
            if optional_string(call, "id")?.is_some_and(|id| !id.is_empty() && id != existing_id)
                || optional_string(function, "name")?.is_some_and(|name| !name.is_empty())
            {
                return Err(AnthropicCodecError::protocol());
            }
        } else {
            let id = string(call, "id")?;
            let name = string(function, "name")?;
            validate_token(id, 200)?;
            validate_token(name, 256)?;
            if self.open_tools.values().any(|existing| existing == id)
                || optional_string(call, "type")?.is_some_and(|kind| kind != "function")
            {
                return Err(AnthropicCodecError::protocol());
            }
            let identity = self.tool_bindings.response_identity(name)?;
            if identity.kind() == crate::ProviderToolKind::Custom {
                self.custom_arguments.insert(index, String::new());
            }
            self.open_tools.insert(index, id.to_owned());
            self.events.push(ProviderStreamEvent::ToolCallStarted {
                index,
                provider_call_id: id.to_owned(),
                identity,
            });
        }
        if let Some(arguments) = optional_string(function, "arguments")? {
            validate_text(arguments)?;
            if let Some(buffer) = self.custom_arguments.get_mut(&index) {
                if buffer.len().saturating_add(arguments.len()) > MAX_REQUEST_TEXT_BYTES {
                    return Err(AnthropicCodecError::size_limit());
                }
                buffer.push_str(arguments);
            } else if !arguments.is_empty() {
                self.events
                    .push(ProviderStreamEvent::ToolCallArgumentsDelta {
                        index,
                        provider_call_id: self.open_tools[&index].clone(),
                        delta: arguments.to_owned(),
                    });
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<ParsedOpenAiStream, AnthropicCodecError> {
        if !self.started {
            return Err(AnthropicCodecError::protocol());
        }
        self.close_reasoning();
        for (index, provider_call_id) in self.open_tools {
            if let Some(arguments) = self.custom_arguments.remove(&index) {
                let input: Value = serde_json::from_str(&arguments)
                    .map_err(|_| AnthropicCodecError::protocol())?;
                let input = object(&input)?;
                exact_keys(input, &["input"])?;
                let delta = string(input, "input")?.to_owned();
                validate_text(&delta)?;
                if delta.is_empty() {
                    return Err(AnthropicCodecError::protocol());
                }
                self.events
                    .push(ProviderStreamEvent::ToolCallArgumentsDelta {
                        index,
                        provider_call_id: provider_call_id.clone(),
                        delta,
                    });
            }
            self.events.push(ProviderStreamEvent::ToolCallEnded {
                index,
                provider_call_id,
            });
        }
        if self.text_open {
            self.events
                .push(ProviderStreamEvent::TextEnded { index: 1 });
        }
        let reason = self
            .finish_reason
            .ok_or_else(AnthropicCodecError::incomplete_stream)?;
        let usage = self
            .usage
            .ok_or_else(AnthropicCodecError::incomplete_stream)?;
        self.events.push(ProviderStreamEvent::Usage(usage));
        self.events.push(ProviderStreamEvent::Finished(reason));
        let terminal = if matches!(
            reason,
            ProviderFinishReason::Interrupted | ProviderFinishReason::Filtered
        ) {
            ProviderGatewayTerminal::Failed {
                failure: crate::ModelAttemptFailureFact::from_stream(
                    crate::ProviderStreamFailureKind::Unknown,
                    crate::ModelExecutionCertainty::AcceptanceUnknown,
                ),
                charge: Some(crate::ProviderGatewayTerminalCharge {
                    usage,
                    actual_cost_micros: self.pricing.cost_micros(usage)?,
                }),
            }
        } else {
            ProviderGatewayTerminal::Completed {
                usage,
                actual_cost_micros: self.pricing.cost_micros(usage)?,
            }
        };
        Ok(ParsedOpenAiStream {
            events: self.events,
            terminal,
        })
    }
}

fn openai_usage(value: &Value) -> Result<ProviderTokenUsage, AnthropicCodecError> {
    let value = object(value)?;
    exact_keys(
        value,
        &[
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "prompt_tokens_details",
            "completion_tokens_details",
            "prompt_cache_hit_tokens",
            "prompt_cache_miss_tokens",
        ],
    )?;
    let prompt = usage_number(value, "prompt_tokens")?;
    let completion = usage_number(value, "completion_tokens")?;
    let mut cached = None;
    let mut cache_write = 0;
    if let Some(details) = value.get("prompt_tokens_details")
        && !details.is_null()
    {
        let details = object(details)?;
        exact_keys(
            details,
            &["cached_tokens", "cache_write_tokens", "audio_tokens"],
        )?;
        cached = optional_usage_number(details, "cached_tokens")?;
        cache_write = optional_usage_number(details, "cache_write_tokens")?.unwrap_or(0);
        if optional_usage_number(details, "audio_tokens")?.is_some_and(|audio| audio > prompt) {
            return Err(AnthropicCodecError::protocol());
        }
    }
    match (
        value.get("prompt_cache_hit_tokens"),
        value.get("prompt_cache_miss_tokens"),
    ) {
        (Some(hit), Some(miss)) => {
            let hit = hit.as_u64().ok_or_else(AnthropicCodecError::protocol)?;
            let miss = miss.as_u64().ok_or_else(AnthropicCodecError::protocol)?;
            if hit.checked_add(miss) != Some(prompt) || cached.is_some_and(|cached| cached != hit) {
                return Err(AnthropicCodecError::protocol());
            }
            cached = Some(hit);
        }
        (None, None) => {}
        _ => return Err(AnthropicCodecError::protocol()),
    }
    if let Some(total) = value.get("total_tokens")
        && total.as_u64() != prompt.checked_add(completion)
    {
        return Err(AnthropicCodecError::protocol());
    }
    let mut reasoning = 0_u64;
    if let Some(details) = value.get("completion_tokens_details")
        && !details.is_null()
    {
        let details = object(details)?;
        if let Some(value) = details.get("reasoning_tokens")
            && !value.is_null()
        {
            reasoning = value.as_u64().ok_or_else(AnthropicCodecError::protocol)?;
        }
    }
    if prompt > MAX_SAFE_INTEGER
        || completion > MAX_SAFE_INTEGER
        || cached.is_some_and(|cached| cached > prompt)
        || cached
            .unwrap_or(0)
            .checked_add(cache_write)
            .is_none_or(|total| total > prompt)
        || reasoning > completion
    {
        return Err(AnthropicCodecError::protocol());
    }
    Ok(ProviderTokenUsage {
        input_tokens: prompt,
        cached_input_tokens: cached,
        cache_write_input_tokens: cache_write,
        output_tokens: completion,
        reasoning_output_tokens: reasoning,
    })
}

fn usage_number(object: &Map<String, Value>, key: &str) -> Result<u64, AnthropicCodecError> {
    required(object, key)?
        .as_u64()
        .ok_or_else(AnthropicCodecError::protocol)
}

fn optional_usage_number(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<u64>, AnthropicCodecError> {
    object
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| value.as_u64().ok_or_else(AnthropicCodecError::protocol))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_anthropic::ProviderTokenPricing;

    fn options() -> AnthropicMessagesOptions {
        AnthropicMessagesOptions {
            max_output_tokens: 4096,
            pricing: ProviderTokenPricing::default(),
        }
    }

    fn envelope(payload: Value) -> OpenAiEnvelope {
        OpenAiEnvelope { data: payload }
    }

    fn canonical_payload() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "requestId": "req-1",
            "provider": "opencode",
            "sessionId": "ses-1",
            "threadId": "thr-1",
            "request": {
                "model": "qwen3.8-flash",
                "instructions": "You are a coding agent.",
                "input": [
                    {"type": "message", "id": "m1", "role": "user", "content": [
                        {"type": "input_text", "text": "fix sum()"}
                    ]}
                ],
                "tools": [
                    {"type": "function", "name": "shell", "description": "run", "parameters": {"type": "object", "properties": {}}}
                ],
                "tool_choice": "auto",
                "parallel_tool_calls": true,
                "stream": true,
                "store": false,
            }
        }))
        .expect("payload")
    }

    #[test]
    fn tool_output_preserves_nul_as_json_text() {
        let mut payload: Value = serde_json::from_slice(&canonical_payload()).unwrap();
        let text = format!("command output:{}done", "\0".repeat(255));
        payload["request"]["input"].as_array_mut().unwrap().extend([
            json!({"type":"function_call", "name":"shell", "call_id":"call-1", "arguments":"{}"}),
            json!({"type":"function_call_output", "call_id":"call-1", "output":text}),
        ]);
        let prepared = prepare_openai_chat_request(
            &serde_json::to_vec(&payload).unwrap(),
            "qwen3.8-flash",
            options(),
        )
        .expect("NUL in tool output is valid JSON text");
        assert!(!prepared.body.contains(&0));
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        let output = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call-1")
            .expect("retained tool output");
        assert_eq!(
            output["content"],
            format!("Tool call source_id: call-1\n{text}")
        );
    }

    #[test]
    fn unadvertised_function_call_survives_stream_and_error_feedback_history() {
        let prepared =
            prepare_openai_chat_request(&canonical_payload(), "qwen3.8-flash", options())
                .expect("prepared");
        let mut parser = OpenAiStreamParser::new(&prepared.tool_bindings, options().pricing, 1);
        parser
            .push(&envelope(json!({"id":"r1", "choices":[{"index":0,"delta":{
            "tool_calls":[{"index":0,"id":"call-1","type":"function","function":{
                "name":"apply_patch","arguments":"{}"}}]},
            "finish_reason":"tool_calls"}], "usage":{"prompt_tokens":1,"completion_tokens":1}})))
            .expect("unknown tool is a model error, not corrupt SSE");
        let parsed = parser.finish().expect("metered completion");
        assert!(parsed.events.iter().any(|event| matches!(event,
            ProviderStreamEvent::ToolCallStarted { identity, .. }
            if identity.name() == "apply_patch" && identity.namespace() == Some("winwincode_unadvertised") && identity.kind() == crate::ProviderToolKind::Function)));
        let mut payload: Value = serde_json::from_slice(&canonical_payload()).expect("payload");
        payload["request"]["input"].as_array_mut().expect("input").extend([
            json!({"type":"function_call","name":"apply_patch","namespace":"winwincode_unadvertised","call_id":"call-1","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"call-1","output":"unsupported call: apply_patch"}),
        ]);
        let feedback = prepare_openai_chat_request(
            &serde_json::to_vec(&payload).expect("payload"),
            "qwen3.8-flash",
            options(),
        )
        .expect("Core error feedback can reach model");
        let body: Value = serde_json::from_slice(&feedback.body).expect("body");
        assert_eq!(body["tools"].as_array().expect("tools").len(), 1);
        assert_eq!(body["tools"][0]["function"]["name"], "shell");
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["name"],
            "apply_patch"
        );
        assert!(
            body["messages"][3]["content"]
                .as_str()
                .expect("feedback")
                .ends_with("unsupported call: apply_patch")
        );
    }

    #[test]
    fn custom_tool_arguments_are_unwrapped_for_the_kernel() {
        let mut payload: Value = serde_json::from_slice(&canonical_payload()).expect("payload");
        payload["request"]["tools"] = json!([{
            "type":"custom", "name":"apply_patch", "description":"Apply patch",
            "format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}
        }]);
        let prepared = prepare_openai_chat_request(
            &serde_json::to_vec(&payload).expect("payload"),
            "qwen3.8-flash",
            options(),
        )
        .expect("prepared");
        let mut parser = OpenAiStreamParser::new(&prepared.tool_bindings, options().pricing, 1);
        parser.push(&envelope(json!({"id":"r1", "choices":[{"index":0,"delta":{
            "tool_calls":[{"index":0,"id":"call-1","type":"function","function":{
                "name":"apply_patch","arguments":"{\"input\":\"*** Begin Patch\\n*** End Patch\"}"}}]},
            "finish_reason":"tool_calls"}], "usage":{"prompt_tokens":1,"completion_tokens":1}})))
            .expect("push");
        let parsed = parser.finish().expect("finish");
        assert!(parsed.events.iter().any(|event| matches!(event,
            ProviderStreamEvent::ToolCallArgumentsDelta { delta, .. }
            if delta == "*** Begin Patch\n*** End Patch")));
    }

    #[test]
    fn openai_request_shape_has_messages_tools_and_stream() {
        let prepared =
            prepare_openai_chat_request(&canonical_payload(), "qwen3.8-flash", options())
                .expect("prepared");
        let body: Value = serde_json::from_slice(&prepared.body).expect("body json");
        assert_eq!(body["model"], "qwen3.8-flash");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "You are a coding agent.");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "fix sum()");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "shell");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn structured_output_is_preserved_by_both_protocols_with_reasoning() {
        let mut request: Value = serde_json::from_slice(&canonical_payload()).expect("payload");
        let schema = json!({"type":"object", "additionalProperties":false,
            "properties":{"verdict":{"type":"string", "enum":["pass","fail"]}}, "required":["verdict"]});
        request["request"]["text"] = json!({"format":{
            "type":"json_schema", "name":"verification", "strict":true, "schema":schema}});
        request["request"]["reasoning"] = json!({"effort":"max"});
        let payload = serde_json::to_vec(&request).expect("payload");
        let openai =
            prepare_openai_chat_request(&payload, "deepseek-flash", options()).expect("openai");
        let body: Value = serde_json::from_slice(&openai.body).expect("body");
        assert_eq!(
            body["response_format"],
            json!({"type":"json_schema", "json_schema":{
            "name":"verification", "strict":true, "schema":schema}})
        );
        assert_eq!(body["reasoning_effort"], "max");
        let anthropic =
            prepare_anthropic_request(&payload, "deepseek-flash", options()).expect("anthropic");
        let body: Value = serde_json::from_slice(&anthropic.body).expect("body");
        assert_eq!(
            body["output_config"],
            json!({"effort":"max", "format":{"type":"json_schema", "schema":schema}})
        );
        for field in ["strict", "schema", "type", "name"] {
            let mut invalid = request.clone();
            invalid["request"]["text"]["format"][field] = Value::Null;
            let payload = serde_json::to_vec(&invalid).expect("payload");
            assert!(prepare_openai_chat_request(&payload, "deepseek-flash", options()).is_err());
            assert!(prepare_anthropic_request(&payload, "deepseek-flash", options()).is_err());
        }
    }

    #[test]
    fn openai_stream_maps_text_tool_calls_usage_and_finish() {
        let prepared =
            prepare_openai_chat_request(&canonical_payload(), "qwen3.8-flash", options())
                .expect("prepared");
        let wire = vec![
            envelope(json!({
                "id": "chatcmpl-1",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hel"}}],
            })),
            envelope(json!({
                "id": "chatcmpl-1",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"content": "lo"}}],
            })),
            envelope(json!({
                "id": "chatcmpl-1",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "shell", "arguments": ""}
                }]}}],
            })),
            envelope(json!({
                "id": "chatcmpl-1",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "{\"cmd\":\"ls\"}"}
                }]}}],
            })),
            envelope(json!({
                "id": "chatcmpl-1",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 11, "completion_tokens": 5, "total_tokens": 16}
            })),
        ];
        let mut parser =
            OpenAiStreamParser::new(&prepared.tool_bindings, options().pricing, wire.len());
        for item in &wire {
            parser.push(item).expect("push");
        }
        let parsed = parser.finish().expect("finish");
        let kinds: Vec<&str> = parsed
            .events
            .iter()
            .map(|event| match event {
                ProviderStreamEvent::ResponseStarted { .. } => "started",
                ProviderStreamEvent::TextStarted { .. } => "text.started",
                ProviderStreamEvent::TextDelta { .. } => "text.delta",
                ProviderStreamEvent::TextEnded { .. } => "text.ended",
                ProviderStreamEvent::ToolCallStarted { .. } => "tool.started",
                ProviderStreamEvent::ToolCallArgumentsDelta { .. } => "tool.args",
                ProviderStreamEvent::ToolCallEnded { .. } => "tool.ended",
                ProviderStreamEvent::Usage(_) => "usage",
                ProviderStreamEvent::Finished(_) => "finished",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "started",
                "text.started",
                "text.delta",
                "text.delta",
                "text.ended",
                "tool.started",
                "tool.args",
                "tool.ended",
                "usage",
                "finished",
            ]
        );
        match &parsed.terminal {
            ProviderGatewayTerminal::Completed { usage, .. } => {
                assert_eq!(usage.input_tokens, 11);
                assert_eq!(usage.output_tokens, 5);
            }
            other => panic!("unexpected terminal {other:?}"),
        }
    }

    #[test]
    fn optional_cache_details_preserve_known_tokens_and_unknown_cost() {
        for details in [
            None,
            Some(Value::Null),
            Some(json!({})),
            Some(json!({"cached_tokens":null})),
        ] {
            let mut value = json!({"prompt_tokens":16,"completion_tokens":10,"total_tokens":26});
            if let Some(details) = details {
                value["prompt_tokens_details"] = details;
            }
            let usage = openai_usage(&value).unwrap();
            assert_eq!(usage.input_tokens, 16);
            assert_eq!(usage.output_tokens, 10);
            assert_eq!(usage.cached_input_tokens, None);
            assert_eq!(
                ProviderTokenPricing::default().cost_micros(usage).unwrap(),
                None
            );
            let pricing = ProviderTokenPricing {
                input_micros_per_million_tokens: 2_000_000,
                cached_input_micros_per_million_tokens: 1_000_000,
                ..Default::default()
            };
            assert_eq!(pricing.cost_micros(usage).unwrap(), None);
            let same = ProviderTokenPricing {
                cached_input_micros_per_million_tokens: 2_000_000,
                ..pricing
            };
            assert_eq!(same.cost_micros(usage).unwrap(), Some(32));
        }
    }

    #[test]
    fn opencode_go_stream_accepts_measured_token_details() {
        use std::fmt::Write as _;
        let prepared =
            prepare_openai_chat_request(&canonical_payload(), "glm-5.3-flash", options()).unwrap();
        let chunks = [
            json!({"id":"go-response", "object":"chat.completion.chunk", "model":"glm-5.3-flash",
                "choices":[{"index":0,"finish_reason":null,"logprobs":null,
                    "delta":{"role":"assistant","content":"","refusal":null}}],"usage":null}),
            json!({"id":"go-response", "choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"go-call","type":"function","function":{"name":"shell","arguments":""}}]}}]}),
            json!({"id":"go-response", "choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"{}"}}]}}]}),
            json!({"id":"go-response", "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"id":"go-response", "choices":[], "usage":{
                "prompt_tokens":167,"completion_tokens":33,"total_tokens":200,
                "prompt_tokens_details":{"audio_tokens":0,"cached_tokens":6,"cache_write_tokens":4},
                "completion_tokens_details":{"accepted_prediction_tokens":0,"audio_tokens":0,
                    "reasoning_tokens":0,"rejected_prediction_tokens":0}}}),
        ];
        let mut wire = String::new();
        for chunk in chunks {
            write!(wire, "data: {chunk}\n\n").unwrap();
        }
        wire.push_str("data: [DONE]\n\n");
        let parsed = parse_openai_chat_sse(
            wire.as_bytes(),
            4096,
            16,
            &prepared.tool_bindings,
            options(),
        )
        .expect("measured Go stream");
        let ProviderGatewayTerminal::Completed { usage, .. } = parsed.terminal else {
            panic!("expected metered completion");
        };
        assert_eq!(usage.input_tokens, 167);
        assert_eq!(usage.cached_input_tokens, Some(6));
        assert_eq!(usage.cache_write_input_tokens, 4);
        assert_eq!(usage.output_tokens, 33);
        assert!(parsed.events.iter().any(|event| matches!(event,
            ProviderStreamEvent::ToolCallArgumentsDelta { delta, .. } if delta == "{}")));
    }

    #[test]
    fn opencode_go_token_details_reject_invalid_or_overlapping_counts() {
        let usage = json!({"prompt_tokens":10,"completion_tokens":3,"total_tokens":13,
            "prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":4,"audio_tokens":0}});
        assert!(openai_usage(&usage).is_ok());
        for value in [json!(-1), json!("4"), json!(5), json!(MAX_SAFE_INTEGER + 1)] {
            let mut invalid = usage.clone();
            invalid["prompt_tokens_details"]["cache_write_tokens"] = value;
            assert!(openai_usage(&invalid).is_err());
        }
        let mut invalid = usage;
        invalid["prompt_tokens_details"]["audio_tokens"] = json!(11);
        assert!(openai_usage(&invalid).is_err());
    }

    #[test]
    fn deepseek_stream_accepts_null_role_in_terminal_delta() {
        let chunk = json!({"id":"deepseek-response", "choices":[{"index":0,
            "delta":{"role":null,"content":null}, "finish_reason":"stop"}],
            "usage":{"prompt_tokens":16,"completion_tokens":10,"total_tokens":26}});
        let wire = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        let parsed = parse_openai_chat_sse(
            wire.as_bytes(),
            2048,
            8,
            &AnthropicToolBindings::default(),
            options(),
        )
        .expect("legal null role");
        assert!(matches!(
            parsed.terminal,
            ProviderGatewayTerminal::Completed { .. }
        ));
    }

    #[test]
    fn deepseek_stream_retains_cache_usage_without_double_counting() {
        let chunk = json!({"id":"deepseek-response", "choices":[{"index":0,
            "delta":{}, "finish_reason":"stop"}],
            "usage":{"prompt_tokens":16,"completion_tokens":10,"total_tokens":26,
                "prompt_tokens_details":{"cached_tokens":6},
                "prompt_cache_hit_tokens":6,"prompt_cache_miss_tokens":10}});
        let wire = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        let parsed = parse_openai_chat_sse(
            wire.as_bytes(),
            2048,
            8,
            &AnthropicToolBindings::default(),
            options(),
        )
        .expect("legal cache usage");
        let ProviderGatewayTerminal::Completed { usage, .. } = parsed.terminal else {
            panic!("expected completion");
        };
        assert_eq!(usage.input_tokens, 16);
        assert_eq!(usage.cached_input_tokens, Some(6));
        assert_eq!(usage.output_tokens, 10);
        for (field, value) in [
            ("prompt_cache_miss_tokens", 11),
            ("prompt_cache_hit_tokens", 7),
            ("total_tokens", 25),
        ] {
            let mut invalid = chunk.clone();
            invalid["usage"][field] = json!(value);
            let wire = format!("data: {invalid}\n\ndata: [DONE]\n\n");
            assert!(
                parse_openai_chat_sse(
                    wire.as_bytes(),
                    2048,
                    8,
                    &AnthropicToolBindings::default(),
                    options()
                )
                .is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn sse_representation_normalizes_metadata_bom_and_all_line_endings() {
        let chunk = json!({"id":"response-1","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}});
        let wire = format!(
            "id: 1\nretry: 1000\n: keepalive\nevent:message\nx-provider-info: ignored\ndata: {chunk}\n\ndata:[DONE]\n\n"
        );
        for ending in ["\n", "\r\n", "\r"] {
            let body = format!("\u{feff}{}", wire.replace('\n', ending));
            let parsed = parse_openai_chat_sse(
                body.as_bytes(),
                2048,
                8,
                &AnthropicToolBindings::default(),
                options(),
            )
            .unwrap();
            assert!(matches!(
                parsed.terminal,
                ProviderGatewayTerminal::Completed { .. }
            ));
        }
    }

    #[test]
    fn rejects_truncated_unmetered_and_multiple_choice_streams() {
        let bindings = AnthropicToolBindings::default();
        let chunk = json!({"id":"response-1","model":"observed-qwen","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}});
        let wire = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        let parsed = parse_openai_chat_sse(wire.as_bytes(), 2048, 8, &bindings, options())
            .expect("complete");
        assert!(
            matches!(&parsed.events[0], ProviderStreamEvent::ResponseStarted { observed_model_id: Some(model), .. } if model == "observed-qwen")
        );
        assert!(
            parse_openai_chat_sse(
                format!("data: {chunk}\n\n").as_bytes(),
                2048,
                8,
                &bindings,
                options()
            )
            .is_err()
        );
        for pointer in ["/usage", "/choices/0/index"] {
            let mut invalid = chunk.clone();
            *invalid.pointer_mut(pointer).unwrap() = if pointer == "/usage" {
                Value::Null
            } else {
                json!(1)
            };
            let wire = format!("data: {invalid}\n\ndata: [DONE]\n\n");
            assert!(parse_openai_chat_sse(wire.as_bytes(), 2048, 8, &bindings, options()).is_err());
        }
    }
}
