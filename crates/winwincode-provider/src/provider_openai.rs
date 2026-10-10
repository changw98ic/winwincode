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
    ProviderTokenPricing, array, exact_keys, normalize_messages_request, object, optional_string,
    required, string, validate_text, validate_token,
};
use crate::{
    ProviderFinishReason, ProviderGatewayTerminal, ProviderStreamEvent, ProviderTokenUsage,
};

const MAX_REQUEST_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[cfg(test)]
#[path = "provider_media_tests.rs"]
mod media_tests;

#[cfg(test)]
#[path = "provider_openai_usage_tests.rs"]
mod usage_tests;

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
    let prepared = normalize_messages_request(payload, upstream_model_id, options)?;
    let normalized = object(&prepared.body)?;
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
        let mut tool_media: Vec<Value> = Vec::new();
        let mut media_parts: Vec<Value> = Vec::new();
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
                    let (text, media) = tool_result_content(content, id)?;
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": text,
                    }));
                    tool_media.extend(media);
                }
                "image" => {
                    media_parts.push(openai_image(block)?);
                }
                "audio" => media_parts.push(crate::provider_media::openai_audio(block)?),
                _ => return Err(AnthropicCodecError::invalid_request()),
            }
        }
        if role == "user" {
            for result in tool_results {
                messages.push(result);
            }
            if !text_parts.is_empty() || !media_parts.is_empty() || !tool_media.is_empty() {
                let mut content = tool_media;
                if !text_parts.is_empty() {
                    if content.is_empty() && media_parts.is_empty() && text_parts.len() == 1 {
                        messages.push(json!({"role": "user", "content": text_parts[0]}));
                        continue;
                    }
                    content.push(json!({"type": "text", "text": text_parts.join("\n")}));
                }
                content.extend(media_parts);
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

fn tool_result_content(
    content: &Value,
    call_id: &str,
) -> Result<(String, Vec<Value>), AnthropicCodecError> {
    if let Some(text) = content.as_str() {
        validate_text(text)?;
        return Ok((text.to_owned(), Vec::new()));
    }
    let blocks = content
        .as_array()
        .ok_or_else(AnthropicCodecError::invalid_request)?;
    let mut parts = Vec::new();
    let mut media = Vec::new();
    let mut has_media = false;
    for block in blocks {
        let block = object(block)?;
        match string(block, "type")? {
            "text" => {
                exact_keys(block, &["type", "text"])?;
                let text = string(block, "text")?;
                validate_text(text)?;
                if !text.is_empty() {
                    parts.push(text.to_owned());
                }
                media.push(json!({"type":"text", "text":text}));
            }
            "image" => {
                has_media = true;
                media.push(openai_image(block)?);
            }
            "audio" => {
                has_media = true;
                media.push(crate::provider_media::openai_audio(block)?);
            }
            _ => return Err(AnthropicCodecError::invalid_request()),
        }
    }
    // Chat tool messages accept text only. A matched receipt precedes the
    // mixed content, whose source marker and block order remain intact.
    if has_media {
        Ok((format!("Tool call source_id: {call_id}"), media))
    } else {
        Ok((parts.join("\n"), Vec::new()))
    }
}

fn openai_image(block: &Map<String, Value>) -> Result<Value, AnthropicCodecError> {
    exact_keys(block, &["type", "source", "detail"])?;
    let source = object(required(block, "source")?)?;
    exact_keys(source, &["type", "media_type", "data"])?;
    if string(source, "type")? != "base64" {
        return Err(AnthropicCodecError::invalid_request());
    }
    let media_type = string(source, "media_type")?;
    let data = string(source, "data")?;
    let mut image = json!({
        "type": "image_url",
        "image_url": {"url": format!("data:{media_type};base64,{data}")},
    });
    if let Some(detail) = block.get("detail") {
        image["image_url"]["detail"] = detail.clone();
    }
    Ok(image)
}

/// Parsed `OpenAI` chat stream: canonical events plus a terminal fact.
pub(crate) struct ParsedOpenAiStream {
    pub events: Vec<ProviderStreamEvent>,
    pub terminal: ProviderGatewayTerminal,
}

/// Retains a real usage fact even when later stream data is incomplete.
///
/// Only a bounded, well-formed prefix from one validated response identity is
/// considered. A usage fact does not establish successful model completion.
pub(crate) fn observed_openai_usage(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
) -> Option<(String, ProviderTokenUsage)> {
    let bytes = match std::str::from_utf8(bytes) {
        Ok(_) => bytes,
        Err(error) if error.error_len().is_none() => &bytes[..error.valid_up_to()],
        Err(_) => return None,
    };
    let frames =
        crate::provider_sse_framing::parse_prefix(bytes, max_event_bytes, max_events).ok()?;
    let mut response_id: Option<String> = None;
    let mut usage = None;
    for frame in frames {
        if frame.data == "[DONE]" {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&frame.data) else {
            break;
        };
        let Some(value) = value.as_object() else {
            break;
        };
        if value
            .get("object")
            .filter(|kind| !kind.is_null())
            .is_some_and(|kind| kind.as_str() != Some("chat.completion.chunk"))
        {
            break;
        }
        if let Some(id) = value.get("id").filter(|id| !id.is_null()) {
            let id = id.as_str()?;
            validate_token(id, 256).ok()?;
            if response_id
                .as_deref()
                .is_some_and(|existing| existing != id)
            {
                return None;
            }
            response_id = Some(id.to_owned());
        } else if response_id.is_none() {
            break;
        }
        if let Some(value) = value.get("usage")
            && !value.is_null()
        {
            let Ok(observed) = openai_usage(value) else {
                break;
            };
            usage = Some(observed);
        }
    }
    Some((response_id?, usage?))
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
    let wire = parse_openai_sse_envelopes(bytes, max_event_bytes, max_events)
        .map_err(|error| error.with_diagnostic("sse_framing", "chat.completion.chunk", "$"))
        .inspect_err(|error| {
            eprintln!("openai_sse_protocol stage=envelope kind={:?}", error.kind());
        })?;
    let mut parser = OpenAiStreamParser::new(tool_bindings, options.pricing, wire.envelopes.len());
    for (index, envelope) in wire.envelopes.iter().enumerate() {
        parser.push(envelope)
        .map_err(|error| error.with_diagnostic("response_fields", "chat.completion.chunk", "$"))
        .inspect_err(|error| {
            let data = envelope.data.as_object();
            let choices = data.and_then(|value| value.get("choices")).and_then(Value::as_array);
            let delta = choices.and_then(|value| value.first())
                .and_then(|value| value.get("delta")).and_then(Value::as_object);
            eprintln!(
                "openai_sse_protocol stage=chunk index={index} kind={:?} done_seen={} choices={} usage={} finish={} tool_calls={} reasoning={} content={}",
                error.kind(), wire.done, choices.map_or(0, Vec::len),
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
    let parsed = parser.finish().inspect_err(|error| {
        eprintln!("openai_sse_protocol stage=finish kind={:?} finish_seen={finished} usage_seen={measured} open_tools={open_tools}", error.kind());
    });
    if !wire.done {
        eprintln!(
            "openai_sse_protocol stage=eof done_seen=false finish_seen={finished} usage_seen={measured} open_tools={open_tools} semantic_complete={}",
            parsed.as_ref().is_ok_and(|parsed| matches!(
                parsed.terminal,
                ProviderGatewayTerminal::Completed { .. }
            )),
        );
    }
    parsed
}

struct OpenAiEnvelope {
    data: Value,
}

struct OpenAiWire {
    envelopes: Vec<OpenAiEnvelope>,
    done: bool,
}

fn parse_openai_sse_envelopes(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
) -> Result<OpenAiWire, AnthropicCodecError> {
    let frames =
        crate::provider_sse_framing::parse(bytes, max_event_bytes, max_events.saturating_add(1))
            .map_err(|error| match error {
                crate::provider_sse_framing::SseFramingError::Utf8 => {
                    if std::str::from_utf8(bytes).is_err_and(|error| error.error_len().is_none()) {
                        AnthropicCodecError::incomplete_stream()
                    } else {
                        AnthropicCodecError::invalid_sse()
                    }
                }
                crate::provider_sse_framing::SseFramingError::SizeLimit => {
                    AnthropicCodecError::size_limit()
                }
            })?;
    let mut envelopes = Vec::new();
    let mut done = false;
    let frame_count = frames.len();
    for (index, frame) in frames.into_iter().enumerate() {
        let mut data = frame.data;
        flush_openai_envelope(
            &mut envelopes,
            &mut data,
            &mut done,
            max_event_bytes,
            max_events,
            index + 1 == frame_count,
        )?;
    }
    // [DONE] is a transport marker. The chunks still have to establish their
    // own completion, valid output and usage, whether this marker is present
    // or the peer closes the response immediately after the terminal chunk.
    Ok(OpenAiWire { envelopes, done })
}

fn flush_openai_envelope(
    envelopes: &mut Vec<OpenAiEnvelope>,
    data: &mut String,
    done: &mut bool,
    max_event_bytes: usize,
    max_events: usize,
    final_frame: bool,
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
        return Err(AnthropicCodecError::protocol().with_diagnostic(
            "response_lifecycle",
            "chat.completion.chunk",
            "$",
        ));
    }
    if data.len() > max_event_bytes || envelopes.len() >= max_events {
        return Err(AnthropicCodecError::size_limit());
    }
    let value: Value = serde_json::from_str(data).map_err(|error| {
        let error = if final_frame && error.is_eof() {
            AnthropicCodecError::incomplete_stream()
        } else {
            AnthropicCodecError::protocol()
        };
        error.with_diagnostic("json_decode", "chat.completion.chunk", "$")
    })?;
    envelopes.push(OpenAiEnvelope { data: value });
    data.clear();
    Ok(())
}

struct OpenAiStreamParser<'a> {
    tool_bindings: &'a AnthropicToolBindings,
    pricing: ProviderTokenPricing,
    events: Vec<ProviderStreamEvent>,
    open_tools: BTreeMap<u32, String>,
    function_arguments: BTreeMap<u32, String>,
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
            function_arguments: BTreeMap::new(),
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
            } else {
                self.function_arguments.insert(index, String::new());
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
            } else if let Some(buffer) = self.function_arguments.get_mut(&index) {
                if buffer.len().saturating_add(arguments.len()) > MAX_REQUEST_TEXT_BYTES {
                    return Err(AnthropicCodecError::size_limit());
                }
                buffer.push_str(arguments);
                if !arguments.is_empty() {
                    self.events
                        .push(ProviderStreamEvent::ToolCallArgumentsDelta {
                            index,
                            provider_call_id: self.open_tools[&index].clone(),
                            delta: arguments.to_owned(),
                        });
                }
            }
        }
        Ok(())
    }

    fn close_tools(&mut self, reason: ProviderFinishReason) -> Result<(), AnthropicCodecError> {
        for (index, provider_call_id) in std::mem::take(&mut self.open_tools) {
            if let Some(arguments) = self.custom_arguments.remove(&index) {
                let input: Value =
                    serde_json::from_str(&arguments).map_err(|_| tool_arguments_error(reason))?;
                let input = object(&input)?;
                let delta = crate::provider_tool_arguments::translated_custom_arguments(
                    input,
                    index,
                    &provider_call_id,
                    &mut self.events,
                )?;
                if !delta.is_empty() {
                    self.events
                        .push(ProviderStreamEvent::ToolCallArgumentsDelta {
                            index,
                            provider_call_id: provider_call_id.clone(),
                            delta,
                        });
                }
            } else if let Some(arguments) = self.function_arguments.remove(&index) {
                let input: Value =
                    serde_json::from_str(&arguments).map_err(|_| tool_arguments_error(reason))?;
                if !input.is_object() {
                    return Err(tool_arguments_error(reason));
                }
            }
            self.events.push(ProviderStreamEvent::ToolCallEnded {
                index,
                provider_call_id,
            });
        }
        Ok(())
    }

    fn finish(mut self) -> Result<ParsedOpenAiStream, AnthropicCodecError> {
        if !self.started {
            return Err(AnthropicCodecError::incomplete_stream().with_diagnostic(
                "response_lifecycle",
                "eof",
                "$.id",
            ));
        }
        let reason = self.finish_reason.ok_or_else(|| {
            AnthropicCodecError::incomplete_stream().with_diagnostic(
                "response_lifecycle",
                "eof",
                "$.choices[0].finish_reason",
            )
        })?;
        let usage = self.usage.ok_or_else(|| {
            AnthropicCodecError::incomplete_stream().with_diagnostic(
                "response_lifecycle",
                "eof",
                "$.usage",
            )
        })?;
        self.close_reasoning();
        self.close_tools(reason)?;
        if self.text_open {
            self.events
                .push(ProviderStreamEvent::TextEnded { index: 1 });
        }
        self.events.push(ProviderStreamEvent::Usage(usage));
        self.events.push(ProviderStreamEvent::Finished(reason));
        let terminal = if matches!(
            reason,
            ProviderFinishReason::Interrupted | ProviderFinishReason::Filtered
        ) {
            ProviderGatewayTerminal::Failed {
                failure: crate::ModelAttemptFailureFact::from_stream(
                    if reason == ProviderFinishReason::Interrupted {
                        crate::ProviderStreamFailureKind::Transport
                    } else {
                        crate::ProviderStreamFailureKind::InvalidRequest
                    },
                    if self.events.iter().any(|event| {
                        matches!(
                            event,
                            ProviderStreamEvent::TextDelta { .. }
                                | ProviderStreamEvent::ReasoningContentDelta { .. }
                                | ProviderStreamEvent::ToolCallStarted { .. }
                        )
                    }) {
                        crate::ModelExecutionCertainty::OutputObserved
                    } else {
                        crate::ModelExecutionCertainty::AcceptanceUnknown
                    },
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

fn tool_arguments_error(reason: ProviderFinishReason) -> AnthropicCodecError {
    let error = if reason == ProviderFinishReason::Interrupted {
        AnthropicCodecError::incomplete_stream()
    } else {
        AnthropicCodecError::protocol()
    };
    error.with_diagnostic(
        "tool_arguments",
        "chat.completion.chunk",
        "$.choices[0].delta.tool_calls[].function.arguments",
    )
}

fn openai_usage(value: &Value) -> Result<ProviderTokenUsage, AnthropicCodecError> {
    // External usage metadata is additive. Validate consumed counters, not
    // an exact upstream field list, for both stream and attempt accounting.
    let value = object(value).map_err(|_| usage_error("$.usage"))?;
    let prompt = usage_number(value, "prompt_tokens", "$.usage.prompt_tokens")?;
    let completion = usage_number(value, "completion_tokens", "$.usage.completion_tokens")?;
    let mut cached = None;
    let mut cache_write = 0;
    if let Some(details) = value.get("prompt_tokens_details")
        && !details.is_null()
    {
        let details = object(details).map_err(|_| usage_error("$.usage.prompt_tokens_details"))?;
        cached = optional_usage_number(
            details,
            "cached_tokens",
            "$.usage.prompt_tokens_details.cached_tokens",
        )?;
        cache_write = optional_usage_number(
            details,
            "cache_write_tokens",
            "$.usage.prompt_tokens_details.cache_write_tokens",
        )?
        .unwrap_or(0);
    }
    match (
        value.get("prompt_cache_hit_tokens"),
        value.get("prompt_cache_miss_tokens"),
    ) {
        (Some(hit), Some(miss)) => {
            let hit = hit
                .as_u64()
                .ok_or_else(|| usage_error("$.usage.prompt_cache_hit_tokens"))?;
            let miss = miss
                .as_u64()
                .ok_or_else(|| usage_error("$.usage.prompt_cache_miss_tokens"))?;
            if hit.checked_add(miss) != Some(prompt) || cached.is_some_and(|cached| cached != hit) {
                return Err(usage_error("$.usage.prompt_cache_hit_tokens"));
            }
            cached = Some(hit);
        }
        (None, None) => {}
        _ => return Err(usage_error("$.usage.prompt_cache_hit_tokens")),
    }
    if let Some(total) = value.get("total_tokens")
        && total.as_u64() != prompt.checked_add(completion)
    {
        return Err(usage_error("$.usage.total_tokens"));
    }
    let mut reasoning = 0_u64;
    if let Some(details) = value.get("completion_tokens_details")
        && !details.is_null()
    {
        let details =
            object(details).map_err(|_| usage_error("$.usage.completion_tokens_details"))?;
        reasoning = optional_usage_number(
            details,
            "reasoning_tokens",
            "$.usage.completion_tokens_details.reasoning_tokens",
        )?
        .unwrap_or(0);
    }
    if prompt
        .checked_sub(cached.unwrap_or(0))
        .and_then(|standard| standard.checked_sub(cache_write))
        .is_none()
        || reasoning > completion
        || prompt
            .checked_add(completion)
            .is_none_or(|total| total > MAX_SAFE_INTEGER)
    {
        return Err(usage_error("$.usage"));
    }
    Ok(ProviderTokenUsage {
        input_tokens: prompt,
        cached_input_tokens: cached,
        cache_write_input_tokens: cache_write,
        output_tokens: completion,
        reasoning_output_tokens: reasoning,
    })
}

fn usage_error(field_path: &'static str) -> AnthropicCodecError {
    AnthropicCodecError::protocol().with_diagnostic(
        "response_fields",
        "chat.completion.chunk",
        field_path,
    )
}

fn usage_number(
    object: &Map<String, Value>,
    key: &str,
    field_path: &'static str,
) -> Result<u64, AnthropicCodecError> {
    optional_usage_number(object, key, field_path)?.ok_or_else(|| usage_error(field_path))
}

fn optional_usage_number(
    object: &Map<String, Value>,
    key: &str,
    field_path: &'static str,
) -> Result<Option<u64>, AnthropicCodecError> {
    object
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_u64()
                .filter(|value| *value <= MAX_SAFE_INTEGER)
                .ok_or_else(|| usage_error(field_path))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_anthropic::ProviderTokenPricing;
    use crate::provider_anthropic::prepare_anthropic_request;

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
    fn code_mode_controls_and_resumed_history_survive_both_provider_routes() {
        let source = "text(await tools.repository__lookup({path:'src/lib.rs'}));";
        let waiting = "Script running with cell ID cell-1";
        let completed = "Script completed";
        let mut payload: Value = serde_json::from_slice(&canonical_payload()).unwrap();
        payload["request"]["tools"] = json!([
            {"type":"custom", "name":"exec", "description":"Run JavaScript",
                "format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}},
            {"type":"function", "name":"wait", "description":"Wait for a cell",
                "parameters":{"type":"object","properties":{"cell_id":{"type":"string"}},"required":["cell_id"]}}
        ]);
        payload["request"]["input"].as_array_mut().unwrap().extend([
            json!({"type":"custom_tool_call","name":"exec","call_id":"exec-1","input":source}),
            json!({"type":"custom_tool_call_output","call_id":"exec-1","output":waiting}),
            json!({"type":"function_call","name":"wait","call_id":"wait-1","arguments":"{\"cell_id\":\"cell-1\"}"}),
            json!({"type":"function_call_output","call_id":"wait-1","output":completed}),
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();
        let openai = prepare_openai_chat_request(&bytes, "fixture-model", options()).unwrap();
        let anthropic = prepare_anthropic_request(&bytes, "fixture-model", options()).unwrap();
        let openai: Value = serde_json::from_slice(&openai.body).unwrap();
        let anthropic: Value = serde_json::from_slice(&anthropic.body).unwrap();
        assert_eq!(
            openai["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["exec", "wait"],
        );
        assert_eq!(
            anthropic["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["exec", "wait"],
        );
        let openai_calls = openai["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
            .map(|call| {
                (
                    call["id"].as_str().unwrap(),
                    call["function"]["arguments"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            openai_calls,
            [
                (
                    "exec-1",
                    serde_json::to_string(&json!({"input":source}))
                        .unwrap()
                        .as_str()
                ),
                ("wait-1", "{\"cell_id\":\"cell-1\"}"),
            ]
        );
        let anthropic_calls = anthropic["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .filter(|block| block["type"] == "tool_use")
            .map(|block| (block["id"].clone(), block["input"].clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            anthropic_calls,
            [
                (json!("exec-1"), json!({"input":source})),
                (json!("wait-1"), json!({"cell_id":"cell-1"})),
            ]
        );
        for body in [&openai, &anthropic] {
            let history = body["messages"].to_string();
            assert!(history.contains(waiting));
            assert!(history.contains(completed));
        }
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
    fn semantic_text_completion_at_eof_does_not_require_done() {
        let bindings = AnthropicToolBindings::default();
        let terminal = json!({"id":"response-1","model":"observed-qwen",
            "choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]});
        let usage = json!({"id":"response-1","choices":[],
            "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}});
        let wire = format!("data: {terminal}\n\ndata: {usage}\n\n");
        let eof = parse_openai_chat_sse(wire.as_bytes(), 2048, 8, &bindings, options())
            .expect("explicit finish and real usage survive transport EOF");
        let done = parse_openai_chat_sse(
            format!("{wire}data: [DONE]\n\n").as_bytes(),
            2048,
            8,
            &bindings,
            options(),
        )
        .expect("normal DONE completion");
        assert_eq!(eof.events, done.events);
        assert_eq!(eof.terminal, done.terminal);
        assert_eq!(
            eof.events
                .iter()
                .filter(|event| matches!(event, ProviderStreamEvent::TextEnded { .. }))
                .count(),
            1
        );
        assert!(matches!(eof.terminal,
            ProviderGatewayTerminal::Completed { usage, .. }
            if usage.input_tokens == 2 && usage.output_tokens == 1));
    }

    #[test]
    fn semantic_tool_completion_at_eof_validates_assembled_arguments() {
        let prepared =
            prepare_openai_chat_request(&canonical_payload(), "qwen3.8-flash", options())
                .expect("prepared");
        let start = json!({"id":"response-1","choices":[{"index":0,"delta":{
            "tool_calls":[{"index":0,"id":"call-1","type":"function","function":{
                "name":"shell","arguments":"{\"cmd\":"}}]}}]});
        let terminal = json!({"id":"response-1","choices":[{"index":0,"delta":{
            "tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]},
            "finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}});
        let wire = format!("data: {start}\n\ndata: {terminal}\n\n");
        let eof =
            parse_openai_chat_sse(wire.as_bytes(), 2048, 8, &prepared.tool_bindings, options())
                .expect("valid complete tool JSON at EOF");
        let done = parse_openai_chat_sse(
            format!("{wire}data: [DONE]\n\n").as_bytes(),
            2048,
            8,
            &prepared.tool_bindings,
            options(),
        )
        .expect("valid normal DONE completion");
        assert_eq!(eof.events, done.events);
        assert_eq!(eof.terminal, done.terminal);
        assert_eq!(
            eof.events
                .iter()
                .filter(|event| matches!(event, ProviderStreamEvent::ToolCallEnded { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn incomplete_eof_and_done_without_semantic_finish_remain_interrupted() {
        let bindings = AnthropicToolBindings::default();
        for delta in [
            json!({"content":"still generating"}),
            json!({"tool_calls":[{"index":0,"id":"call-1","type":"function",
                "function":{"name":"shell","arguments":"{\"cmd\":"}}]}),
        ] {
            let chunk = json!({"id":"response-1","choices":[{"index":0,
                "delta":delta,"finish_reason":null}],
                "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}});
            for ending in ["", "data: [DONE]\n\n"] {
                let wire = format!("data: {chunk}\n\n{ending}");
                let error = parse_openai_chat_sse(wire.as_bytes(), 2048, 8, &bindings, options())
                    .err()
                    .expect("finish cannot be inferred from EOF or usage");
                assert_eq!(
                    error.kind(),
                    crate::provider_anthropic::AnthropicCodecErrorKind::IncompleteStream
                );
                assert_eq!(
                    error.diagnostic().expect("safe EOF diagnostic").field_path,
                    "$.choices[0].finish_reason"
                );
            }
        }
        for wire in ["", "data: [DONE]\n\n"] {
            let error = parse_openai_chat_sse(wire.as_bytes(), 2048, 8, &bindings, options())
                .err()
                .expect("an empty response cannot finish");
            assert_eq!(
                error.kind(),
                crate::provider_anthropic::AnthropicCodecErrorKind::IncompleteStream
            );
        }
    }

    #[test]
    fn invalid_tool_json_is_never_closed_as_a_successful_call() {
        let prepared =
            prepare_openai_chat_request(&canonical_payload(), "qwen3.8-flash", options())
                .expect("prepared");
        for arguments in ["", "{\"cmd\":", "{bad}", "[]", "null"] {
            let chunk = json!({"id":"response-1","choices":[{"index":0,
                "delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function",
                    "function":{"name":"shell","arguments":arguments}}]},
                "finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}});
            for ending in ["", "data: [DONE]\n\n"] {
                let wire = format!("data: {chunk}\n\n{ending}");
                let error = parse_openai_chat_sse(
                    wire.as_bytes(),
                    2048,
                    8,
                    &prepared.tool_bindings,
                    options(),
                )
                .err()
                .expect("explicit finish does not make invalid tool JSON complete");
                assert_eq!(
                    error.kind(),
                    crate::provider_anthropic::AnthropicCodecErrorKind::Protocol
                );
                let diagnostic = error.diagnostic().expect("safe tool JSON diagnostic");
                assert_eq!(diagnostic.stage, "tool_arguments");
                assert_eq!(
                    diagnostic.field_path,
                    "$.choices[0].delta.tool_calls[].function.arguments"
                );
            }
        }
    }

    #[test]
    fn explicit_interruption_and_filter_remain_typed_failed_attempts_with_real_usage() {
        for (finish_reason, failure_kind) in [
            ("aborted", crate::ModelAttemptFailureKind::Transport),
            (
                "insufficient_system_resource",
                crate::ModelAttemptFailureKind::Transport,
            ),
            (
                "content_filter",
                crate::ModelAttemptFailureKind::InvalidRequest,
            ),
        ] {
            for content in [Value::Null, json!("partial output")] {
                let chunk = json!({"id":"response-1","choices":[{"index":0,
                    "delta":{"content":content},"finish_reason":finish_reason}],
                    "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}});
                let wire = format!("data: {chunk}\n\n");
                let parsed = parse_openai_chat_sse(
                    wire.as_bytes(),
                    2048,
                    8,
                    &AnthropicToolBindings::default(),
                    options(),
                )
                .expect("upstream explicitly reported a failed terminal");
                let ProviderGatewayTerminal::Failed {
                    failure,
                    charge: Some(charge),
                } = parsed.terminal
                else {
                    panic!("interrupted/filtered output must not succeed");
                };
                assert_eq!(failure.kind, failure_kind);
                assert_eq!(
                    failure.certainty,
                    if content.is_null() {
                        crate::ModelExecutionCertainty::AcceptanceUnknown
                    } else {
                        crate::ModelExecutionCertainty::OutputObserved
                    }
                );
                assert_eq!(charge.usage.input_tokens, 2);
                assert_eq!(charge.usage.output_tokens, 1);
            }
        }
    }

    #[test]
    fn observed_usage_survives_interruption_without_implying_completion() {
        let chunk = json!({"id":"response-1","choices":[{"index":0,
            "delta":{"content":"partial"},"finish_reason":null}],
            "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}});
        let wire = format!("data: {chunk}\n\ndata: {{\"id\":");
        let (id, usage) = observed_openai_usage(wire.as_bytes(), 2048, 8)
            .expect("usage supplied before truncated JSON is a real usage fact");
        assert_eq!(id, "response-1");
        assert_eq!(usage.input_tokens, 2);
        assert_eq!(usage.output_tokens, 1);
        let mut utf8_tail = format!("data: {chunk}\n\ndata: {{\"content\":\"").into_bytes();
        utf8_tail.extend_from_slice(&[0xe2, 0x82]);
        let (_, prefix_usage) = observed_openai_usage(&utf8_tail, 2048, 8)
            .expect("a partial UTF-8 tail cannot erase earlier real usage");
        assert_eq!(prefix_usage, usage);
        let utf8_error = parse_openai_chat_sse(
            &utf8_tail,
            2048,
            8,
            &AnthropicToolBindings::default(),
            options(),
        )
        .err()
        .expect("a partial UTF-8 tail cannot complete");
        assert_eq!(
            utf8_error.kind(),
            crate::provider_anthropic::AnthropicCodecErrorKind::IncompleteStream
        );
        let error = parse_openai_chat_sse(
            wire.as_bytes(),
            2048,
            8,
            &AnthropicToolBindings::default(),
            options(),
        )
        .err()
        .expect("truncated JSON cannot complete");
        assert_eq!(
            error.kind(),
            crate::provider_anthropic::AnthropicCodecErrorKind::IncompleteStream
        );
        assert_eq!(
            error.diagnostic().expect("JSON EOF diagnostic").stage,
            "json_decode"
        );
        let different = json!({"id":"response-2","choices":[]});
        let ambiguous = format!("data: {chunk}\n\ndata: {different}\n\n");
        assert!(observed_openai_usage(ambiguous.as_bytes(), 2048, 8).is_none());
        for wire in ["", "data: [DONE]\n\n", "data: {\"id\":\"response-1\"}\n\n"] {
            assert!(observed_openai_usage(wire.as_bytes(), 2048, 8).is_none());
        }
        let invalid = json!({"id":"response-1","choices":[],
            "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":4}});
        assert!(
            observed_openai_usage(format!("data: {invalid}\n\n").as_bytes(), 2048, 8).is_none()
        );
    }

    #[test]
    fn observed_openai_usage_survives_framing_limits_without_stream_success() {
        let chunk = json!({"id":"response-1","choices":[{"index":0,
            "delta":{"content":"partial"},"finish_reason":null}],
            "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}});
        let prefix = format!("data: {chunk}\n\n");
        let observed = observed_openai_usage(prefix.as_bytes(), 2048, 1).unwrap();
        for suffix in [
            format!("data: {}\n\n", "x".repeat(2049)),
            "data: {}\n\ndata: {}\n\n".into(),
        ] {
            let bytes = format!("{prefix}{suffix}");
            assert_eq!(
                observed_openai_usage(bytes.as_bytes(), 2048, 1),
                Some(observed.clone())
            );
            let error = parse_openai_chat_sse(
                bytes.as_bytes(),
                2048,
                1,
                &AnthropicToolBindings::default(),
                options(),
            )
            .err()
            .expect("framing violation cannot succeed");
            assert_eq!(
                error.kind(),
                crate::provider_anthropic::AnthropicCodecErrorKind::SizeLimit
            );
        }
        assert!(observed_openai_usage(prefix.as_bytes(), 2048, 0).is_none());
        let different = json!({"id":"response-2","choices":[]});
        let drifted = format!(
            "{prefix}data: {different}\n\ndata: {}\n\n",
            "x".repeat(2049)
        );
        assert!(observed_openai_usage(drifted.as_bytes(), 2048, 8).is_none());
        let mut invalid = prefix.into_bytes();
        invalid.push(0xff);
        assert!(observed_openai_usage(&invalid, 2048, 1).is_none());
    }

    #[test]
    fn rejects_unmetered_multiple_choice_and_invalid_terminal_order_streams() {
        let bindings = AnthropicToolBindings::default();
        let chunk = json!({"id":"response-1","model":"observed-qwen","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}});
        let wire = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        let parsed = parse_openai_chat_sse(wire.as_bytes(), 2048, 8, &bindings, options())
            .expect("complete");
        assert!(
            matches!(&parsed.events[0], ProviderStreamEvent::ResponseStarted { observed_model_id: Some(model), .. } if model == "observed-qwen")
        );
        for ending in ["data: [DONE]\n\n", "data: {\"id\":\"response-1\"}\n\n"] {
            assert!(
                parse_openai_chat_sse(
                    format!("{wire}{ending}").as_bytes(),
                    2048,
                    8,
                    &bindings,
                    options(),
                )
                .is_err()
            );
        }
        for pointer in ["/usage", "/choices/0/index"] {
            let mut invalid = chunk.clone();
            *invalid.pointer_mut(pointer).unwrap() = if pointer == "/usage" {
                Value::Null
            } else {
                json!(1)
            };
            let wire = format!("data: {invalid}\n\ndata: [DONE]\n\n");
            assert!(parse_openai_chat_sse(wire.as_bytes(), 2048, 8, &bindings, options()).is_err());
            let eof = format!("data: {invalid}\n\n");
            let error = parse_openai_chat_sse(eof.as_bytes(), 2048, 8, &bindings, options())
                .err()
                .expect("EOF does not bypass required metering/choice checks");
            if pointer == "/usage" {
                assert_eq!(
                    error.kind(),
                    crate::provider_anthropic::AnthropicCodecErrorKind::IncompleteStream
                );
                assert_eq!(
                    error.diagnostic().expect("usage diagnostic").field_path,
                    "$.usage"
                );
            }
        }
    }
}
