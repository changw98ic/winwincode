// SPDX-License-Identifier: Apache-2.0

//! Anthropic Messages protocol translation for the external HTTPS/SSE adapter.
//!
//! This module is the only boundary which understands both the Codex
//! `ModelStreamRequest` JSON shape and Anthropic Messages request/stream
//! shapes. It never receives a Provider Credential and never retains request
//! or response bodies after one conversion.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::{
    ProviderFinishReason, ProviderGatewayTerminal, ProviderStreamEvent, ProviderStreamFailure,
    ProviderStreamFailureKind, ProviderTokenUsage, ProviderToolIdentity, ProviderToolKind,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_REQUEST_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_MESSAGES: usize = 16_384;
const MAX_TOOLS: usize = 1_024;
const MAX_TOOL_ARGUMENT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProviderTokenPricing {
    pub input_micros_per_million_tokens: u64,
    pub cached_input_micros_per_million_tokens: u64,
    pub cache_write_micros_per_million_tokens: u64,
    pub output_micros_per_million_tokens: u64,
    pub reasoning_output_micros_per_million_tokens: u64,
}

impl ProviderTokenPricing {
    pub(crate) fn validate(self) -> Result<(), AnthropicCodecError> {
        if [
            self.input_micros_per_million_tokens,
            self.cached_input_micros_per_million_tokens,
            self.cache_write_micros_per_million_tokens,
            self.output_micros_per_million_tokens,
            self.reasoning_output_micros_per_million_tokens,
        ]
        .into_iter()
        .any(|value| value > MAX_SAFE_INTEGER)
        {
            return Err(AnthropicCodecError::invalid_request());
        }
        Ok(())
    }

    fn cost_micros(self, usage: ProviderTokenUsage) -> Result<u64, AnthropicCodecError> {
        let standard_input_tokens = usage
            .input_tokens
            .checked_sub(usage.cached_input_tokens)
            .and_then(|value| value.checked_sub(usage.cache_write_input_tokens))
            .ok_or_else(AnthropicCodecError::protocol)?;
        let priced = [
            (standard_input_tokens, self.input_micros_per_million_tokens),
            (
                usage.cached_input_tokens,
                self.cached_input_micros_per_million_tokens,
            ),
            (
                usage.cache_write_input_tokens,
                self.cache_write_micros_per_million_tokens,
            ),
            (usage.output_tokens, self.output_micros_per_million_tokens),
            (
                usage.reasoning_output_tokens,
                self.reasoning_output_micros_per_million_tokens,
            ),
        ];
        let numerator = priced
            .into_iter()
            .try_fold(0_u128, |total, (tokens, rate)| {
                total.checked_add(u128::from(tokens) * u128::from(rate))
            });
        let value = numerator
            .and_then(|value| value.checked_div(1_000_000))
            .and_then(|value| u64::try_from(value).ok())
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .ok_or_else(AnthropicCodecError::protocol)?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AnthropicMessagesOptions {
    pub max_output_tokens: u32,
    pub pricing: ProviderTokenPricing,
}

impl AnthropicMessagesOptions {
    pub(crate) fn validate(self) -> Result<(), AnthropicCodecError> {
        if self.max_output_tokens == 0 || u64::from(self.max_output_tokens) > MAX_SAFE_INTEGER {
            return Err(AnthropicCodecError::invalid_request());
        }
        self.pricing.validate()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnthropicCodecErrorKind {
    InvalidRequest,
    Protocol,
    SizeLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AnthropicCodecError {
    kind: AnthropicCodecErrorKind,
}

impl AnthropicCodecError {
    const fn invalid_request() -> Self {
        Self {
            kind: AnthropicCodecErrorKind::InvalidRequest,
        }
    }

    const fn protocol() -> Self {
        Self {
            kind: AnthropicCodecErrorKind::Protocol,
        }
    }

    const fn size_limit() -> Self {
        Self {
            kind: AnthropicCodecErrorKind::SizeLimit,
        }
    }

    pub(crate) const fn kind(self) -> AnthropicCodecErrorKind {
        self.kind
    }
}

pub(crate) struct PreparedAnthropicRequest {
    pub body: Vec<u8>,
    pub tool_bindings: AnthropicToolBindings,
}

#[derive(Clone, Default)]
pub(crate) struct AnthropicToolBindings {
    by_exposed_name: BTreeMap<String, ProviderToolIdentity>,
    by_identity: BTreeMap<ProviderToolIdentity, String>,
}

impl AnthropicToolBindings {
    fn insert(
        &mut self,
        exposed_name: String,
        identity: ProviderToolIdentity,
    ) -> Result<(), AnthropicCodecError> {
        if self.by_exposed_name.contains_key(&exposed_name)
            || self.by_identity.contains_key(&identity)
        {
            return Err(AnthropicCodecError::invalid_request());
        }
        self.by_identity
            .insert(identity.clone(), exposed_name.clone());
        self.by_exposed_name.insert(exposed_name, identity);
        Ok(())
    }

    fn identity(&self, exposed_name: &str) -> Option<&ProviderToolIdentity> {
        self.by_exposed_name.get(exposed_name)
    }

    fn exposed_name(&self, identity: &ProviderToolIdentity) -> Option<&str> {
        self.by_identity.get(identity).map(String::as_str)
    }
}

pub(crate) struct ParsedAnthropicStream {
    pub events: Vec<ProviderStreamEvent>,
    pub terminal: ProviderGatewayTerminal,
}

pub(crate) fn prepare_anthropic_request(
    payload: &[u8],
    upstream_model_id: &str,
    options: AnthropicMessagesOptions,
) -> Result<PreparedAnthropicRequest, AnthropicCodecError> {
    options.validate()?;
    validate_token(upstream_model_id, 256)?;
    if has_local_context_annotation(upstream_model_id) {
        return Err(AnthropicCodecError::invalid_request());
    }
    let root: Value =
        serde_json::from_slice(payload).map_err(|_| AnthropicCodecError::invalid_request())?;
    let root = object(&root)?;
    exact_keys(
        root,
        &[
            "requestId",
            "provider",
            "sessionId",
            "threadId",
            "turnId",
            "request",
        ],
    )?;
    for key in ["requestId", "provider", "sessionId", "threadId"] {
        validate_token(string(root, key)?, 512)?;
    }
    if let Some(turn_id) = optional_string(root, "turnId")? {
        validate_token(turn_id, 512)?;
    }

    let request = object(required(root, "request")?)?;
    exact_keys(
        request,
        &[
            "model",
            "instructions",
            "input",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "reasoning",
            "store",
            "stream",
            "stream_options",
            "include",
            "service_tier",
            "prompt_cache_key",
            "text",
            "client_metadata",
        ],
    )?;
    validate_token(string(request, "model")?, 256)?;
    let instructions = string(request, "instructions")?;
    validate_text(instructions)?;
    if !boolean(request, "stream")? || boolean(request, "store")? {
        return Err(AnthropicCodecError::invalid_request());
    }
    let reasoning_effort = validate_request_controls(request)?;

    let (tools, tool_bindings) = translate_tools(request.get("tools"))?;
    let tool_choice = translate_tool_choice(
        string(request, "tool_choice")?,
        boolean(request, "parallel_tool_calls")?,
        !tools.is_empty(),
    )?;
    let (messages, extra_system) = translate_messages(array(request, "input")?, &tool_bindings)?;
    if messages.is_empty() || messages.len() > MAX_MESSAGES {
        return Err(AnthropicCodecError::invalid_request());
    }

    let mut system = instructions.to_owned();
    for value in extra_system {
        if !system.is_empty() {
            system.push_str("\n\n");
        }
        system.push_str(&value);
        validate_text(&system)?;
    }
    let mut body = Map::new();
    body.insert("model".to_owned(), upstream_model_id.to_owned().into());
    body.insert(
        "max_tokens".to_owned(),
        Value::Number(options.max_output_tokens.into()),
    );
    body.insert("stream".to_owned(), Value::Bool(true));
    insert_reasoning_controls(&mut body, reasoning_effort);
    if !system.is_empty() {
        body.insert("system".to_owned(), Value::String(system));
    }
    body.insert("messages".to_owned(), Value::Array(messages));
    if !tools.is_empty() && string(request, "tool_choice")? != "none" {
        body.insert("tools".to_owned(), Value::Array(tools));
        if let Some(tool_choice) = tool_choice {
            body.insert("tool_choice".to_owned(), tool_choice);
        }
    }
    let body = serde_json::to_vec(&Value::Object(body))
        .map_err(|_| AnthropicCodecError::invalid_request())?;
    if body.len() > MAX_REQUEST_TEXT_BYTES {
        return Err(AnthropicCodecError::size_limit());
    }
    Ok(PreparedAnthropicRequest {
        body,
        tool_bindings,
    })
}

fn insert_reasoning_controls(body: &mut Map<String, Value>, effort: Option<&str>) {
    if let Some(effort) = effort {
        body.insert("thinking".to_owned(), json!({"type": "adaptive"}));
        body.insert("output_config".to_owned(), json!({"effort": effort}));
    }
}

fn validate_request_controls(
    request: &Map<String, Value>,
) -> Result<Option<&str>, AnthropicCodecError> {
    if request.get("text").is_some_and(|value| !value.is_null()) {
        return Err(AnthropicCodecError::invalid_request());
    }
    let mut effort = None;
    if let Some(reasoning) = request.get("reasoning")
        && !reasoning.is_null()
    {
        let reasoning = object(reasoning)?;
        exact_keys(reasoning, &["effort", "summary", "context"])?;
        effort = optional_string(reasoning, "effort")?;
        if effort.is_some_and(|value| !matches!(value, "low" | "medium" | "high" | "xhigh" | "max"))
        {
            return Err(AnthropicCodecError::invalid_request());
        }
        let _ = optional_string(reasoning, "summary")?;
        let _ = optional_string(reasoning, "context")?;
    }
    if let Some(stream_options) = request.get("stream_options")
        && !stream_options.is_null()
        && !stream_options.is_object()
    {
        return Err(AnthropicCodecError::invalid_request());
    }
    if let Some(include) = request.get("include") {
        let include = include
            .as_array()
            .ok_or_else(AnthropicCodecError::invalid_request)?;
        if include.iter().any(|value| !value.is_string()) {
            return Err(AnthropicCodecError::invalid_request());
        }
    }
    for key in ["service_tier", "prompt_cache_key"] {
        let _ = optional_string(request, key)?;
    }
    if let Some(metadata) = request.get("client_metadata")
        && !metadata.is_null()
    {
        let metadata = object(metadata)?;
        if metadata.iter().any(|(key, value)| {
            validate_token(key, 256).is_err()
                || value
                    .as_str()
                    .is_none_or(|value| validate_text(value).is_err())
        }) {
            return Err(AnthropicCodecError::invalid_request());
        }
    }
    Ok(effort)
}

fn translate_tools(
    value: Option<&Value>,
) -> Result<(Vec<Value>, AnthropicToolBindings), AnthropicCodecError> {
    let Some(value) = value else {
        return Ok((Vec::new(), AnthropicToolBindings::default()));
    };
    if value.is_null() {
        return Ok((Vec::new(), AnthropicToolBindings::default()));
    }
    let values = value
        .as_array()
        .ok_or_else(AnthropicCodecError::invalid_request)?;
    let mut tools = Vec::new();
    let mut bindings = AnthropicToolBindings::default();
    let mut canonical_names = BTreeSet::new();
    let mut used_exposed_names = BTreeSet::new();
    for value in values {
        let tool = object(value)?;
        if string(tool, "type")? == "namespace" {
            exact_keys(tool, &["type", "name", "description", "tools"])?;
            let namespace = string(tool, "name")?;
            let namespace_description = optional_string(tool, "description")?.unwrap_or_default();
            validate_text(namespace_description)?;
            let children = array(tool, "tools")?;
            if children.is_empty() {
                return Err(AnthropicCodecError::invalid_request());
            }
            ProviderToolIdentity::try_new(
                ProviderToolKind::Function,
                "tool".to_owned(),
                Some(namespace.to_owned()),
            )
            .map_err(|_| AnthropicCodecError::invalid_request())?;
            for child in children {
                tools.push(translate_tool(
                    object(child)?,
                    Some(namespace),
                    namespace_description,
                    &mut bindings,
                    &mut canonical_names,
                    &mut used_exposed_names,
                )?);
                if tools.len() > MAX_TOOLS {
                    return Err(AnthropicCodecError::size_limit());
                }
            }
        } else {
            tools.push(translate_tool(
                tool,
                None,
                "",
                &mut bindings,
                &mut canonical_names,
                &mut used_exposed_names,
            )?);
            if tools.len() > MAX_TOOLS {
                return Err(AnthropicCodecError::size_limit());
            }
        }
    }
    Ok((tools, bindings))
}

fn translate_tool(
    tool: &Map<String, Value>,
    namespace: Option<&str>,
    namespace_description: &str,
    bindings: &mut AnthropicToolBindings,
    canonical_names: &mut BTreeSet<(Option<String>, String)>,
    used_exposed_names: &mut BTreeSet<String>,
) -> Result<Value, AnthropicCodecError> {
    let kind = match string(tool, "type")? {
        "function" => ProviderToolKind::Function,
        "custom" => ProviderToolKind::Custom,
        _ => return Err(AnthropicCodecError::invalid_request()),
    };
    let name = string(tool, "name")?;
    let identity =
        ProviderToolIdentity::try_new(kind, name.to_owned(), namespace.map(str::to_owned))
            .map_err(|_| AnthropicCodecError::invalid_request())?;
    if !canonical_names.insert((namespace.map(str::to_owned), name.to_owned())) {
        return Err(AnthropicCodecError::invalid_request());
    }
    let description = optional_string(tool, "description")?.unwrap_or_default();
    validate_text(description)?;
    let description = qualified_tool_description(namespace_description, description)?;
    let candidate = namespace.map_or_else(|| name.to_owned(), |value| format!("{value}__{name}"));
    let exposed_name = unique_exposed_tool_name(&candidate, used_exposed_names);
    let translated = match kind {
        ProviderToolKind::Function => {
            exact_keys(
                tool,
                &[
                    "type",
                    "name",
                    "description",
                    "strict",
                    "defer_loading",
                    "parameters",
                    "output_schema",
                ],
            )?;
            validate_optional_boolean(tool, "strict")?;
            validate_optional_boolean(tool, "defer_loading")?;
            if tool
                .get("output_schema")
                .is_some_and(|value| !value.is_null())
            {
                return Err(AnthropicCodecError::invalid_request());
            }
            let parameters = required(tool, "parameters")?;
            if !parameters.is_object() {
                return Err(AnthropicCodecError::invalid_request());
            }
            json!({
                "name": exposed_name,
                "description": description,
                "input_schema": parameters,
            })
        }
        ProviderToolKind::Custom => {
            exact_keys(
                tool,
                &["type", "name", "description", "defer_loading", "format"],
            )?;
            validate_optional_boolean(tool, "defer_loading")?;
            let format = object(required(tool, "format")?)?;
            exact_keys(format, &["type", "syntax", "definition"])?;
            for key in ["type", "syntax", "definition"] {
                validate_text(string(format, key)?)?;
            }
            json!({
                "name": exposed_name,
                "description": description,
                "input_schema": {
                    "type": "object",
                    "properties": {"input": {"type": "string"}},
                    "required": ["input"],
                    "additionalProperties": false,
                },
            })
        }
    };
    bindings.insert(exposed_name, identity)?;
    Ok(translated)
}

fn qualified_tool_description(
    namespace_description: &str,
    tool_description: &str,
) -> Result<String, AnthropicCodecError> {
    let description = if namespace_description.is_empty() {
        tool_description.to_owned()
    } else if tool_description.is_empty() {
        namespace_description.to_owned()
    } else {
        format!("{namespace_description}\n\n{tool_description}")
    };
    validate_text(&description)?;
    Ok(description)
}

fn unique_exposed_tool_name(candidate: &str, used: &mut BTreeSet<String>) -> String {
    let base = candidate.chars().take(56).collect::<String>();
    let mut exposed = base.clone();
    let mut suffix = 2_u32;
    while !used.insert(exposed.clone()) {
        let stem = base.chars().take(52).collect::<String>();
        exposed = format!("{stem}_{suffix}");
        suffix += 1;
    }
    exposed
}

fn translate_tool_choice(
    value: &str,
    parallel: bool,
    has_tools: bool,
) -> Result<Option<Value>, AnthropicCodecError> {
    if !has_tools || value == "none" {
        return match value {
            "auto" | "none" => Ok(None),
            _ => Err(AnthropicCodecError::invalid_request()),
        };
    }
    let kind = match value {
        "auto" => "auto",
        "required" => "any",
        _ => return Err(AnthropicCodecError::invalid_request()),
    };
    let mut choice = Map::new();
    choice.insert("type".to_owned(), Value::String(kind.to_owned()));
    if !parallel {
        choice.insert("disable_parallel_tool_use".to_owned(), Value::Bool(true));
    }
    Ok(Some(Value::Object(choice)))
}

fn translate_messages(
    input: &[Value],
    tool_bindings: &AnthropicToolBindings,
) -> Result<(Vec<Value>, Vec<String>), AnthropicCodecError> {
    if input.is_empty() || input.len() > MAX_MESSAGES {
        return Err(AnthropicCodecError::invalid_request());
    }
    let mut messages = Vec::new();
    let mut system = Vec::new();
    let mut known_calls = BTreeSet::new();
    for item in input {
        let item = object(item)?;
        let kind = string(item, "type")?;
        match kind {
            "message" => translate_message(item, &mut messages, &mut system)?,
            "function_call" => {
                translate_function_call(item, tool_bindings, &mut known_calls, &mut messages)?;
            }
            "custom_tool_call" => {
                translate_custom_tool_call(item, tool_bindings, &mut known_calls, &mut messages)?;
            }
            "function_call_output" | "custom_tool_call_output" => {
                translate_tool_output(kind, item, &known_calls, &mut messages)?;
            }
            "reasoning" => translate_reasoning_item(item, &mut messages)?,
            _ => return Err(AnthropicCodecError::invalid_request()),
        }
    }
    if messages
        .first()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        != Some("user")
    {
        return Err(AnthropicCodecError::invalid_request());
    }
    Ok((messages, system))
}

fn translate_function_call(
    item: &Map<String, Value>,
    tool_bindings: &AnthropicToolBindings,
    known_calls: &mut BTreeSet<String>,
    messages: &mut Vec<Value>,
) -> Result<(), AnthropicCodecError> {
    exact_keys(
        item,
        &[
            "type",
            "id",
            "name",
            "namespace",
            "arguments",
            "encrypted_function_args",
            "call_id",
            "internal_chat_message_metadata_passthrough",
        ],
    )?;
    let name = string(item, "name")?;
    let exposed_name = exposed_history_tool_name(
        tool_bindings,
        ProviderToolKind::Function,
        name,
        optional_string(item, "namespace")?,
    )?;
    let call_id = string(item, "call_id")?;
    validate_token(call_id, 200)?;
    let arguments = string(item, "arguments")?;
    let input: Value =
        serde_json::from_str(arguments).map_err(|_| AnthropicCodecError::invalid_request())?;
    if !input.is_object() || !known_calls.insert(call_id.to_owned()) {
        return Err(AnthropicCodecError::invalid_request());
    }
    push_message_block(
        messages,
        "assistant",
        json!({"type":"tool_use", "id":call_id, "name":exposed_name, "input":input}),
    )
}

fn translate_custom_tool_call(
    item: &Map<String, Value>,
    tool_bindings: &AnthropicToolBindings,
    known_calls: &mut BTreeSet<String>,
    messages: &mut Vec<Value>,
) -> Result<(), AnthropicCodecError> {
    exact_keys(
        item,
        &[
            "type",
            "id",
            "status",
            "call_id",
            "name",
            "namespace",
            "input",
            "internal_chat_message_metadata_passthrough",
        ],
    )?;
    let name = string(item, "name")?;
    let exposed_name = exposed_history_tool_name(
        tool_bindings,
        ProviderToolKind::Custom,
        name,
        optional_string(item, "namespace")?,
    )?;
    let call_id = string(item, "call_id")?;
    validate_token(call_id, 200)?;
    let input = string(item, "input")?;
    validate_text(input)?;
    if !known_calls.insert(call_id.to_owned()) {
        return Err(AnthropicCodecError::invalid_request());
    }
    push_message_block(
        messages,
        "assistant",
        json!({"type":"tool_use", "id":call_id, "name":exposed_name, "input":{"input":input}}),
    )
}

fn translate_tool_output(
    kind: &str,
    item: &Map<String, Value>,
    known_calls: &BTreeSet<String>,
    messages: &mut Vec<Value>,
) -> Result<(), AnthropicCodecError> {
    let allowed = if kind == "function_call_output" {
        &[
            "type",
            "id",
            "call_id",
            "output",
            "internal_chat_message_metadata_passthrough",
        ][..]
    } else {
        &[
            "type",
            "id",
            "call_id",
            "name",
            "output",
            "internal_chat_message_metadata_passthrough",
        ][..]
    };
    exact_keys(item, allowed)?;
    let call_id = string(item, "call_id")?;
    validate_token(call_id, 200)?;
    if !known_calls.contains(call_id) {
        return Err(AnthropicCodecError::invalid_request());
    }
    let output = tool_output(required(item, "output")?)?;
    push_message_block(
        messages,
        "user",
        json!({"type":"tool_result", "tool_use_id":call_id, "content":output}),
    )
}

fn translate_reasoning_item(
    item: &Map<String, Value>,
    messages: &mut Vec<Value>,
) -> Result<(), AnthropicCodecError> {
    exact_keys(
        item,
        &[
            "type",
            "id",
            "summary",
            "content",
            "encrypted_content",
            "internal_chat_message_metadata_passthrough",
        ],
    )?;
    if item
        .get("encrypted_content")
        .is_some_and(|value| !value.is_null())
    {
        return Err(AnthropicCodecError::invalid_request());
    }
    let mut fragments = Vec::new();
    append_reasoning_fragments(item, "summary", "summary_text", &mut fragments)?;
    append_reasoning_fragments(item, "content", "reasoning_text", &mut fragments)?;
    if fragments.is_empty() {
        return Ok(());
    }
    let text = fragments.join("\n");
    validate_text(&text)?;
    // Anthropic requires provider-signed thinking blocks when replaying prior
    // thinking. Canonical Codex history does not carry that signature, so the
    // validated visible reasoning is retained as assistant text instead of
    // forging or dropping a thinking block. Adjacent tool calls are appended
    // to this same assistant message in their original order.
    push_message_block(messages, "assistant", json!({"type":"text", "text":text}))
}

fn append_reasoning_fragments(
    item: &Map<String, Value>,
    field: &str,
    expected_kind: &str,
    fragments: &mut Vec<String>,
) -> Result<(), AnthropicCodecError> {
    let Some(values) = item.get(field) else {
        return Ok(());
    };
    if values.is_null() {
        return Ok(());
    }
    let values = values
        .as_array()
        .ok_or_else(AnthropicCodecError::invalid_request)?;
    for value in values {
        let value = object(value)?;
        exact_keys(value, &["type", "text"])?;
        if string(value, "type")? != expected_kind {
            return Err(AnthropicCodecError::invalid_request());
        }
        let text = string(value, "text")?;
        validate_text(text)?;
        if !text.is_empty() {
            fragments.push(text.to_owned());
        }
    }
    Ok(())
}

fn exposed_history_tool_name<'bindings>(
    tool_bindings: &'bindings AnthropicToolBindings,
    kind: ProviderToolKind,
    name: &str,
    namespace: Option<&str>,
) -> Result<&'bindings str, AnthropicCodecError> {
    let identity =
        ProviderToolIdentity::try_new(kind, name.to_owned(), namespace.map(str::to_owned))
            .map_err(|_| AnthropicCodecError::invalid_request())?;
    tool_bindings
        .exposed_name(&identity)
        .ok_or_else(AnthropicCodecError::invalid_request)
}

fn translate_message(
    item: &Map<String, Value>,
    messages: &mut Vec<Value>,
    system: &mut Vec<String>,
) -> Result<(), AnthropicCodecError> {
    exact_keys(
        item,
        &[
            "type",
            "id",
            "role",
            "content",
            "phase",
            "internal_chat_message_metadata_passthrough",
        ],
    )?;
    let role = string(item, "role")?;
    let content = array(item, "content")?;
    let mut blocks = Vec::with_capacity(content.len());
    for value in content {
        let value = object(value)?;
        exact_keys(value, &["type", "text"])?;
        match string(value, "type")? {
            "input_text" | "output_text" => {
                let text = string(value, "text")?;
                validate_text(text)?;
                if !text.is_empty() {
                    blocks.push(json!({"type":"text", "text":text}));
                }
            }
            _ => return Err(AnthropicCodecError::invalid_request()),
        }
    }
    match role {
        "developer" | "system" => {
            if blocks.is_empty() {
                return Err(AnthropicCodecError::invalid_request());
            }
            system.push(
                blocks
                    .iter()
                    .map(|block| string(object(block)?, "text"))
                    .collect::<Result<Vec<_>, _>>()?
                    .join("\n"),
            );
        }
        "user" | "assistant" => {
            for block in blocks {
                push_message_block(messages, role, block)?;
            }
        }
        _ => return Err(AnthropicCodecError::invalid_request()),
    }
    Ok(())
}

fn tool_output(value: &Value) -> Result<Value, AnthropicCodecError> {
    if let Some(text) = value.as_str() {
        validate_text(text)?;
        return Ok(Value::String(text.to_owned()));
    }
    let values = value
        .as_array()
        .ok_or_else(AnthropicCodecError::invalid_request)?;
    let mut blocks = Vec::with_capacity(values.len());
    for value in values {
        let value = object(value)?;
        exact_keys(value, &["type", "text"])?;
        if string(value, "type")? != "input_text" {
            return Err(AnthropicCodecError::invalid_request());
        }
        let text = string(value, "text")?;
        validate_text(text)?;
        blocks.push(json!({"type":"text", "text":text}));
    }
    Ok(Value::Array(blocks))
}

fn push_message_block(
    messages: &mut Vec<Value>,
    role: &str,
    block: Value,
) -> Result<(), AnthropicCodecError> {
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
    {
        last.get_mut("content")
            .and_then(Value::as_array_mut)
            .ok_or_else(AnthropicCodecError::invalid_request)?
            .push(block);
        return Ok(());
    }
    messages.push(json!({"role": role, "content": [block]}));
    Ok(())
}

pub(crate) fn parse_anthropic_sse(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
    tool_bindings: &AnthropicToolBindings,
    options: AnthropicMessagesOptions,
) -> Result<ParsedAnthropicStream, AnthropicCodecError> {
    options.validate()?;
    let wire = parse_sse_envelopes(bytes, max_event_bytes, max_events)?;
    let mut parser = AnthropicStreamParser::new(tool_bindings, options.pricing, wire.len());
    for envelope in &wire {
        parser.push(envelope)?;
    }
    parser.finish()
}

struct SseEnvelope {
    event: Option<String>,
    data: Value,
}

fn parse_sse_envelopes(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
) -> Result<Vec<SseEnvelope>, AnthropicCodecError> {
    if bytes.contains(&0) {
        return Err(AnthropicCodecError::protocol());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| AnthropicCodecError::protocol())?;
    let normalized = text.replace("\r\n", "\n");
    if normalized.contains('\r') {
        return Err(AnthropicCodecError::protocol());
    }
    let mut envelopes = Vec::new();
    let mut event = None;
    let mut data = String::new();
    for line in normalized.split('\n') {
        if line.len() > max_event_bytes {
            return Err(AnthropicCodecError::size_limit());
        }
        if line.is_empty() {
            dispatch_envelope(
                &mut envelopes,
                &mut event,
                &mut data,
                max_event_bytes,
                max_events,
            )?;
        } else if line.starts_with(':') {
        } else if let Some(value) = line.strip_prefix("event:") {
            if event.is_some() || !data.is_empty() {
                return Err(AnthropicCodecError::protocol());
            }
            let value = value.strip_prefix(' ').unwrap_or(value);
            validate_token(value, 128)?;
            event = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
            if data.len() > max_event_bytes {
                return Err(AnthropicCodecError::size_limit());
            }
        } else {
            return Err(AnthropicCodecError::protocol());
        }
    }
    dispatch_envelope(
        &mut envelopes,
        &mut event,
        &mut data,
        max_event_bytes,
        max_events,
    )?;
    Ok(envelopes)
}

fn dispatch_envelope(
    envelopes: &mut Vec<SseEnvelope>,
    event: &mut Option<String>,
    data: &mut String,
    max_event_bytes: usize,
    max_events: usize,
) -> Result<(), AnthropicCodecError> {
    if data.is_empty() {
        if event.take().is_some() {
            return Err(AnthropicCodecError::protocol());
        }
        return Ok(());
    }
    if data.len() > max_event_bytes || envelopes.len() >= max_events || data == "[DONE]" {
        return Err(
            if data.len() > max_event_bytes || envelopes.len() >= max_events {
                AnthropicCodecError::size_limit()
            } else {
                AnthropicCodecError::protocol()
            },
        );
    }
    let value: Value = serde_json::from_str(data).map_err(|_| AnthropicCodecError::protocol())?;
    let event_name = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(AnthropicCodecError::protocol)?;
    if event.as_deref().is_some_and(|value| value != event_name) {
        return Err(AnthropicCodecError::protocol());
    }
    envelopes.push(SseEnvelope {
        event: event.take(),
        data: value,
    });
    data.clear();
    Ok(())
}

enum OpenAnthropicBlock {
    Text,
    Thinking,
    Tool {
        call_id: String,
        identity: ProviderToolIdentity,
        partial_json: String,
    },
}

struct AnthropicStreamParser<'a> {
    tool_bindings: &'a AnthropicToolBindings,
    pricing: ProviderTokenPricing,
    events: Vec<ProviderStreamEvent>,
    blocks: BTreeMap<u32, OpenAnthropicBlock>,
    usage: Option<ProviderTokenUsage>,
    finish_reason: Option<ProviderFinishReason>,
    started: bool,
    terminal: Option<ProviderGatewayTerminal>,
}

impl<'a> AnthropicStreamParser<'a> {
    fn new(
        tool_bindings: &'a AnthropicToolBindings,
        pricing: ProviderTokenPricing,
        capacity: usize,
    ) -> Self {
        Self {
            tool_bindings,
            pricing,
            events: Vec::with_capacity(capacity.saturating_add(2)),
            blocks: BTreeMap::new(),
            usage: None,
            finish_reason: None,
            started: false,
            terminal: None,
        }
    }

    fn push(&mut self, envelope: &SseEnvelope) -> Result<(), AnthropicCodecError> {
        if self.terminal.is_some() {
            return Err(AnthropicCodecError::protocol());
        }
        let _ = &envelope.event;
        let value = object(&envelope.data)?;
        match string(value, "type")? {
            "ping" => {
                exact_keys(value, &["type"])?;
                if !self.started {
                    return Err(AnthropicCodecError::protocol());
                }
            }
            "message_start" => self.message_start(value)?,
            "content_block_start" => self.content_block_start(value)?,
            "content_block_delta" => self.content_block_delta(value)?,
            "content_block_stop" => self.content_block_stop(value)?,
            "message_delta" => self.message_delta(value)?,
            "message_stop" => self.message_stop(value)?,
            "error" => self.error(value)?,
            _ => return Err(AnthropicCodecError::protocol()),
        }
        Ok(())
    }

    fn message_start(&mut self, value: &Map<String, Value>) -> Result<(), AnthropicCodecError> {
        exact_keys(value, &["type", "message"])?;
        if self.started {
            return Err(AnthropicCodecError::protocol());
        }
        let message = object(required(value, "message")?)?;
        let response_id = string(message, "id")?;
        validate_token(response_id, 200)?;
        if string(message, "type")? != "message" || string(message, "role")? != "assistant" {
            return Err(AnthropicCodecError::protocol());
        }
        let usage = anthropic_usage(required(message, "usage")?, None)?;
        self.usage = Some(usage);
        self.started = true;
        self.events.push(ProviderStreamEvent::ResponseStarted {
            provider_response_id: response_id.to_owned(),
        });
        Ok(())
    }

    fn content_block_start(
        &mut self,
        value: &Map<String, Value>,
    ) -> Result<(), AnthropicCodecError> {
        exact_keys(value, &["type", "index", "content_block"])?;
        self.require_started()?;
        let index = index(value)?;
        if self.blocks.contains_key(&index) {
            return Err(AnthropicCodecError::protocol());
        }
        let block = object(required(value, "content_block")?)?;
        let open = match string(block, "type")? {
            "text" => {
                if block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_none_or(|value| !value.is_empty())
                {
                    return Err(AnthropicCodecError::protocol());
                }
                self.events.push(ProviderStreamEvent::TextStarted { index });
                OpenAnthropicBlock::Text
            }
            "thinking" | "redacted_thinking" => {
                self.events.push(ProviderStreamEvent::ReasoningStarted {
                    index,
                    summary_index: 0,
                });
                OpenAnthropicBlock::Thinking
            }
            "tool_use" => {
                let call_id = string(block, "id")?;
                let name = string(block, "name")?;
                validate_token(call_id, 200)?;
                validate_tool_name(name)?;
                let identity = self
                    .tool_bindings
                    .identity(name)
                    .cloned()
                    .ok_or_else(AnthropicCodecError::protocol)?;
                let input = required(block, "input")?;
                if !input.is_object() {
                    return Err(AnthropicCodecError::protocol());
                }
                let partial_json = if input.as_object().is_some_and(Map::is_empty) {
                    String::new()
                } else {
                    serde_json::to_string(input).map_err(|_| AnthropicCodecError::protocol())?
                };
                self.events.push(ProviderStreamEvent::ToolCallStarted {
                    index,
                    provider_call_id: call_id.to_owned(),
                    identity: identity.clone(),
                });
                OpenAnthropicBlock::Tool {
                    call_id: call_id.to_owned(),
                    identity,
                    partial_json,
                }
            }
            _ => return Err(AnthropicCodecError::protocol()),
        };
        self.blocks.insert(index, open);
        Ok(())
    }

    fn content_block_delta(
        &mut self,
        value: &Map<String, Value>,
    ) -> Result<(), AnthropicCodecError> {
        exact_keys(value, &["type", "index", "delta"])?;
        self.require_started()?;
        let index = index(value)?;
        let delta = object(required(value, "delta")?)?;
        let delta_type = string(delta, "type")?;
        match (self.blocks.get_mut(&index), delta_type) {
            (Some(OpenAnthropicBlock::Text), "text_delta") => {
                let text = string(delta, "text")?;
                if !text.is_empty() {
                    validate_text(text)?;
                    self.events.push(ProviderStreamEvent::TextDelta {
                        index,
                        delta: text.to_owned(),
                    });
                }
            }
            (Some(OpenAnthropicBlock::Thinking), "thinking_delta") => {
                let thinking = string(delta, "thinking")?;
                if !thinking.is_empty() {
                    validate_text(thinking)?;
                    self.events
                        .push(ProviderStreamEvent::ReasoningContentDelta {
                            index,
                            content_index: 0,
                            delta: thinking.to_owned(),
                        });
                }
            }
            (Some(OpenAnthropicBlock::Thinking), "signature_delta") => {
                validate_text(string(delta, "signature")?)?;
            }
            (Some(OpenAnthropicBlock::Tool { partial_json, .. }), "input_json_delta") => {
                let value = string(delta, "partial_json")?;
                if partial_json.len().saturating_add(value.len()) > MAX_TOOL_ARGUMENT_BYTES {
                    return Err(AnthropicCodecError::size_limit());
                }
                partial_json.push_str(value);
            }
            _ => return Err(AnthropicCodecError::protocol()),
        }
        Ok(())
    }

    fn content_block_stop(
        &mut self,
        value: &Map<String, Value>,
    ) -> Result<(), AnthropicCodecError> {
        exact_keys(value, &["type", "index"])?;
        self.require_started()?;
        let index = index(value)?;
        match self
            .blocks
            .remove(&index)
            .ok_or_else(AnthropicCodecError::protocol)?
        {
            OpenAnthropicBlock::Text => {
                self.events.push(ProviderStreamEvent::TextEnded { index });
            }
            OpenAnthropicBlock::Thinking => {
                self.events
                    .push(ProviderStreamEvent::ReasoningEnded { index });
            }
            OpenAnthropicBlock::Tool {
                call_id,
                identity,
                partial_json,
            } => {
                let input = if partial_json.is_empty() {
                    Value::Object(Map::new())
                } else {
                    serde_json::from_str::<Value>(&partial_json)
                        .map_err(|_| AnthropicCodecError::protocol())?
                };
                let input = input
                    .as_object()
                    .ok_or_else(AnthropicCodecError::protocol)?;
                let arguments = match identity.kind() {
                    ProviderToolKind::Function => {
                        serde_json::to_string(input).map_err(|_| AnthropicCodecError::protocol())?
                    }
                    ProviderToolKind::Custom => {
                        exact_keys(input, &["input"])?;
                        string(input, "input")?.to_owned()
                    }
                };
                if arguments.is_empty() {
                    return Err(AnthropicCodecError::protocol());
                }
                self.events
                    .push(ProviderStreamEvent::ToolCallArgumentsDelta {
                        index,
                        provider_call_id: call_id.clone(),
                        delta: arguments,
                    });
                self.events.push(ProviderStreamEvent::ToolCallEnded {
                    index,
                    provider_call_id: call_id,
                });
            }
        }
        Ok(())
    }

    fn message_delta(&mut self, value: &Map<String, Value>) -> Result<(), AnthropicCodecError> {
        exact_keys(value, &["type", "delta", "usage"])?;
        self.require_started()?;
        if !self.blocks.is_empty() || self.finish_reason.is_some() {
            return Err(AnthropicCodecError::protocol());
        }
        let delta = object(required(value, "delta")?)?;
        let stop_reason = string(delta, "stop_reason")?;
        self.finish_reason = Some(match stop_reason {
            "end_turn" | "stop_sequence" => ProviderFinishReason::Stop,
            "tool_use" => ProviderFinishReason::ToolCalls,
            "max_tokens" => ProviderFinishReason::MaxTokens,
            _ => return Err(AnthropicCodecError::protocol()),
        });
        let previous = self.usage.ok_or_else(AnthropicCodecError::protocol)?;
        self.usage = Some(anthropic_usage(required(value, "usage")?, Some(previous))?);
        Ok(())
    }

    fn message_stop(&mut self, value: &Map<String, Value>) -> Result<(), AnthropicCodecError> {
        exact_keys(value, &["type"])?;
        self.require_started()?;
        if !self.blocks.is_empty() {
            return Err(AnthropicCodecError::protocol());
        }
        let usage = self.usage.ok_or_else(AnthropicCodecError::protocol)?;
        let reason = self
            .finish_reason
            .ok_or_else(AnthropicCodecError::protocol)?;
        self.events.push(ProviderStreamEvent::Usage(usage));
        self.events.push(ProviderStreamEvent::Finished(reason));
        self.terminal = Some(ProviderGatewayTerminal::Completed {
            usage,
            actual_cost_micros: self.pricing.cost_micros(usage)?,
        });
        Ok(())
    }

    fn error(&mut self, value: &Map<String, Value>) -> Result<(), AnthropicCodecError> {
        exact_keys(value, &["type", "error"])?;
        let error = object(required(value, "error")?)?;
        exact_keys(error, &["type", "message"])?;
        validate_text(string(error, "message")?)?;
        let kind = match string(error, "type")? {
            "authentication_error" | "permission_error" => {
                ProviderStreamFailureKind::Authentication
            }
            "invalid_request_error" | "not_found_error" => {
                ProviderStreamFailureKind::InvalidRequest
            }
            "rate_limit_error" => ProviderStreamFailureKind::RateLimit,
            "overloaded_error" | "api_error" => ProviderStreamFailureKind::Server,
            _ => ProviderStreamFailureKind::Unknown,
        };
        let failure = ProviderStreamFailure::new(kind);
        self.events
            .push(ProviderStreamEvent::Failed(failure.clone()));
        self.terminal = Some(ProviderGatewayTerminal::Failed {
            failure: crate::ModelAttemptFailureFact::from_stream(
                failure.kind(),
                crate::ModelExecutionCertainty::AcceptanceUnknown,
            ),
            charge: None,
        });
        Ok(())
    }

    fn require_started(&self) -> Result<(), AnthropicCodecError> {
        if !self.started {
            return Err(AnthropicCodecError::protocol());
        }
        Ok(())
    }

    fn finish(self) -> Result<ParsedAnthropicStream, AnthropicCodecError> {
        let terminal = self.terminal.ok_or_else(AnthropicCodecError::protocol)?;
        Ok(ParsedAnthropicStream {
            events: self.events,
            terminal,
        })
    }
}

fn anthropic_usage(
    value: &Value,
    previous: Option<ProviderTokenUsage>,
) -> Result<ProviderTokenUsage, AnthropicCodecError> {
    let value = object(value)?;
    exact_keys(
        value,
        &[
            "input_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
            "output_tokens",
            "server_tool_use",
            "service_tier",
        ],
    )?;
    validate_usage_extensions(value)?;
    let prior = previous.unwrap_or(ProviderTokenUsage {
        input_tokens: 0,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 0,
        reasoning_output_tokens: 0,
    });
    let prior_standard_input = prior
        .input_tokens
        .checked_sub(prior.cached_input_tokens)
        .and_then(|value| value.checked_sub(prior.cache_write_input_tokens))
        .ok_or_else(AnthropicCodecError::protocol)?;
    let standard_input_tokens = usage_counter(
        value,
        "input_tokens",
        previous.is_none(),
        prior_standard_input,
    )?;
    let cached_input_tokens = usage_counter(
        value,
        "cache_read_input_tokens",
        false,
        prior.cached_input_tokens,
    )?;
    let cache_write_input_tokens = usage_counter(
        value,
        "cache_creation_input_tokens",
        false,
        prior.cache_write_input_tokens,
    )?;
    let output_tokens = usage_counter(
        value,
        "output_tokens",
        previous.is_none(),
        prior.output_tokens,
    )?;
    if previous.is_some()
        && (standard_input_tokens < prior_standard_input
            || cached_input_tokens < prior.cached_input_tokens
            || cache_write_input_tokens < prior.cache_write_input_tokens
            || output_tokens < prior.output_tokens)
    {
        return Err(AnthropicCodecError::protocol());
    }
    let input_tokens = standard_input_tokens
        .checked_add(cached_input_tokens)
        .and_then(|value| value.checked_add(cache_write_input_tokens))
        .ok_or_else(AnthropicCodecError::protocol)?;
    let usage = ProviderTokenUsage {
        input_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        output_tokens,
        reasoning_output_tokens: 0,
    };
    if [
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.cache_write_input_tokens,
        usage.output_tokens,
    ]
    .into_iter()
    .any(|value| value > MAX_SAFE_INTEGER)
    {
        return Err(AnthropicCodecError::protocol());
    }
    Ok(usage)
}

fn validate_usage_extensions(value: &Map<String, Value>) -> Result<(), AnthropicCodecError> {
    if let Some(service_tier) = optional_string(value, "service_tier")? {
        validate_token(service_tier, 128)?;
    }
    if let Some(server_tool_use) = value.get("server_tool_use") {
        if server_tool_use.is_null() {
            return Ok(());
        }
        let counters = object(server_tool_use)?;
        for (name, count) in counters {
            validate_token(name, 128)?;
            if count.as_u64() != Some(0) {
                return Err(AnthropicCodecError::protocol());
            }
        }
    }
    Ok(())
}

fn usage_counter(
    usage: &Map<String, Value>,
    key: &str,
    required: bool,
    fallback: u64,
) -> Result<u64, AnthropicCodecError> {
    match optional_u64(usage, key)? {
        Some(value) => Ok(value),
        None if required => Err(AnthropicCodecError::protocol()),
        None => Ok(fallback),
    }
}

fn exact_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), AnthropicCodecError> {
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(AnthropicCodecError::invalid_request());
    }
    Ok(())
}

fn object(value: &Value) -> Result<&Map<String, Value>, AnthropicCodecError> {
    value
        .as_object()
        .ok_or_else(AnthropicCodecError::invalid_request)
}

fn required<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a Value, AnthropicCodecError> {
    object
        .get(key)
        .ok_or_else(AnthropicCodecError::invalid_request)
}

fn string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, AnthropicCodecError> {
    required(object, key)?
        .as_str()
        .ok_or_else(AnthropicCodecError::invalid_request)
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, AnthropicCodecError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(AnthropicCodecError::invalid_request),
    }
}

fn validate_optional_boolean(
    object: &Map<String, Value>,
    key: &str,
) -> Result<(), AnthropicCodecError> {
    if object
        .get(key)
        .is_some_and(|value| !value.is_null() && !value.is_boolean())
    {
        return Err(AnthropicCodecError::invalid_request());
    }
    Ok(())
}

fn boolean(object: &Map<String, Value>, key: &str) -> Result<bool, AnthropicCodecError> {
    required(object, key)?
        .as_bool()
        .ok_or_else(AnthropicCodecError::invalid_request)
}

fn array<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a [Value], AnthropicCodecError> {
    required(object, key)?
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(AnthropicCodecError::invalid_request)
}

fn optional_u64(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<u64>, AnthropicCodecError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(AnthropicCodecError::protocol),
    }
}

fn index(object: &Map<String, Value>) -> Result<u32, AnthropicCodecError> {
    required(object, "index")?
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(AnthropicCodecError::protocol)
}

fn validate_text(value: &str) -> Result<(), AnthropicCodecError> {
    if value.len() > MAX_REQUEST_TEXT_BYTES || value.contains('\0') {
        return Err(AnthropicCodecError::size_limit());
    }
    Ok(())
}

fn validate_token(value: &str, max_len: usize) -> Result<(), AnthropicCodecError> {
    if value.is_empty()
        || value.len() > max_len
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(AnthropicCodecError::invalid_request());
    }
    Ok(())
}

fn validate_tool_name(value: &str) -> Result<(), AnthropicCodecError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(AnthropicCodecError::invalid_request());
    }
    Ok(())
}

fn has_local_context_annotation(value: &str) -> bool {
    let Some(open) = value.rfind('[') else {
        return false;
    };
    let Some(annotation) = value.get(open + 1..value.len().saturating_sub(1)) else {
        return false;
    };
    value.ends_with(']')
        && annotation.len() >= 2
        && annotation
            .get(..annotation.len() - 1)
            .is_some_and(|digits| digits.bytes().all(|byte| byte.is_ascii_digit()))
        && annotation
            .as_bytes()
            .last()
            .is_some_and(|unit| matches!(unit, b'k' | b'K' | b'm' | b'M'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> AnthropicMessagesOptions {
        AnthropicMessagesOptions {
            max_output_tokens: 4096,
            pricing: ProviderTokenPricing {
                input_micros_per_million_tokens: 1_000_000,
                cached_input_micros_per_million_tokens: 100_000,
                cache_write_micros_per_million_tokens: 2_000_000,
                output_micros_per_million_tokens: 3_000_000,
                reasoning_output_micros_per_million_tokens: 0,
            },
        }
    }

    fn bindings(entries: &[(&str, ProviderToolKind)]) -> AnthropicToolBindings {
        let mut bindings = AnthropicToolBindings::default();
        for (name, kind) in entries {
            bindings
                .insert(
                    (*name).to_owned(),
                    ProviderToolIdentity::try_new(*kind, (*name).to_owned(), None)
                        .expect("test tool identity"),
                )
                .expect("unique test binding");
        }
        bindings
    }

    fn canonical_request() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "requestId": "codex-request-1",
            "provider": "winwincode",
            "sessionId": "session-1",
            "threadId": "thread-1",
            "turnId": "turn-1",
            "request": {
                "model": "local-model[1m]",
                "instructions": "Follow the repository instructions.",
                "input": [
                    {"type":"message", "role":"developer", "content":[{"type":"input_text", "text":"Keep the result concise."}]},
                    {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"Inspect the code."}]},
                    {"type":"reasoning", "id":"reasoning-1", "summary":[{"type":"summary_text", "text":"Inspect both files first."}], "content":[{"type":"reasoning_text", "text":"Keep the calls parallel."}], "encrypted_content":null},
                    {"type":"function_call", "name":"read_file", "arguments":"{\"path\":\"src/lib.rs\"}", "call_id":"call-1"},
                    {"type":"custom_tool_call", "call_id":"call-2", "name":"apply_patch", "input":"*** Begin Patch"},
                    {"type":"function_call_output", "call_id":"call-1", "output":"file contents"},
                    {"type":"custom_tool_call_output", "call_id":"call-2", "name":"apply_patch", "output":"Done"},
                    {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"Continue."}]}
                ],
                "tools": [
                    {"type":"function", "name":"read_file", "description":"Read a file", "strict":false, "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},
                    {"type":"custom", "name":"apply_patch", "description":"Apply a patch", "format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}}
                ],
                "tool_choice": "auto",
                "parallel_tool_calls": true,
                "reasoning": {"effort":"high", "summary":"auto"},
                "store": false,
                "stream": true,
                "stream_options": {"include_usage":true},
                "include": [],
                "service_tier": null,
                "prompt_cache_key": "cache-1",
                "text": null,
                "client_metadata": {"source":"test"}
            }
        }))
        .expect("canonical request JSON")
    }

    #[test]
    fn canonical_codex_request_translates_to_exact_upstream_model_and_anthropic_tools() {
        let prepared = prepare_anthropic_request(&canonical_request(), "glm-5.2", options())
            .expect("translate canonical request");
        let body: Value = serde_json::from_slice(&prepared.body).expect("Anthropic request JSON");
        assert_eq!(body["model"], "glm-5.2");
        assert!(!String::from_utf8_lossy(&prepared.body).contains("[1m]"));
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["output_config"], json!({"effort": "high"}));
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert_eq!(
            body["system"],
            "Follow the repository instructions.\n\nKeep the result concise."
        );
        assert_eq!(body["messages"].as_array().expect("messages").len(), 3);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(
            body["messages"][0]["content"][0]["text"],
            "Inspect the code."
        );
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["messages"][1]["content"][0]["type"], "text");
        assert_eq!(
            body["messages"][1]["content"][0]["text"],
            "Inspect both files first.\nKeep the calls parallel."
        );
        assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(body["messages"][1]["content"][1]["id"], "call-1");
        assert_eq!(body["messages"][1]["content"][2]["type"], "tool_use");
        assert_eq!(body["messages"][1]["content"][2]["id"], "call-2");
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call-1");
        assert_eq!(
            body["messages"][2]["content"][0]["content"],
            "file contents"
        );
        assert_eq!(body["messages"][2]["content"][1]["tool_use_id"], "call-2");
        assert_eq!(body["messages"][2]["content"][1]["content"], "Done");
        assert_eq!(body["messages"][2]["content"][2]["text"], "Continue.");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["tools"][1]["input_schema"]["required"][0], "input");
        assert_eq!(body["tool_choice"]["type"], "auto");
        assert!(
            body["tool_choice"]
                .get("disable_parallel_tool_use")
                .is_none()
        );
        assert_eq!(
            prepared
                .tool_bindings
                .identity("apply_patch")
                .map(ProviderToolIdentity::kind),
            Some(ProviderToolKind::Custom)
        );
        let Err(error) = prepare_anthropic_request(&canonical_request(), "glm-5.2[1m]", options())
        else {
            panic!("local context annotation is never an upstream model ID");
        };
        assert_eq!(error.kind(), AnthropicCodecErrorKind::InvalidRequest);
    }

    #[test]
    fn tool_choice_and_unsupported_tool_controls_are_explicit_and_fail_closed() {
        let mut required: Value =
            serde_json::from_slice(&canonical_request()).expect("canonical request value");
        required["request"]["tool_choice"] = Value::String("required".to_owned());
        required["request"]["parallel_tool_calls"] = Value::Bool(false);
        let required = serde_json::to_vec(&required).expect("required request JSON");
        let prepared = prepare_anthropic_request(&required, "glm-5.2", options())
            .expect("required tool request");
        let body: Value = serde_json::from_slice(&prepared.body).expect("Anthropic request JSON");
        assert_eq!(body["tool_choice"]["type"], "any");
        assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], true);

        let mut serial_auto: Value =
            serde_json::from_slice(&canonical_request()).expect("canonical request value");
        serial_auto["request"]["parallel_tool_calls"] = Value::Bool(false);
        let serial_auto = serde_json::to_vec(&serial_auto).expect("serial auto request JSON");
        let prepared = prepare_anthropic_request(&serial_auto, "glm-5.2", options())
            .expect("serial auto tool request");
        let body: Value = serde_json::from_slice(&prepared.body).expect("Anthropic request JSON");
        assert_eq!(body["tool_choice"]["type"], "auto");
        assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], true);

        let mut none: Value =
            serde_json::from_slice(&canonical_request()).expect("canonical request value");
        none["request"]["tool_choice"] = Value::String("none".to_owned());
        let none = serde_json::to_vec(&none).expect("no-tool request JSON");
        let prepared =
            prepare_anthropic_request(&none, "glm-5.2", options()).expect("disabled tool request");
        let body: Value = serde_json::from_slice(&prepared.body).expect("Anthropic request JSON");
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());

        let mut auto_without_tools: Value =
            serde_json::from_slice(&canonical_request()).expect("canonical request value");
        auto_without_tools["request"]
            .as_object_mut()
            .expect("canonical request object")
            .remove("tools");
        auto_without_tools["request"]["input"] = json!([
            {"type":"message", "role":"user", "content":[
                {"type":"input_text", "text":"Inspect the code."}
            ]}
        ]);
        let auto_without_tools =
            serde_json::to_vec(&auto_without_tools).expect("tool-free auto request JSON");
        let prepared = prepare_anthropic_request(&auto_without_tools, "glm-5.2", options())
            .expect("tool-free auto request");
        let body: Value = serde_json::from_slice(&prepared.body).expect("Anthropic request JSON");
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());

        let mut invalid: Value =
            serde_json::from_slice(&canonical_request()).expect("canonical request value");
        invalid["request"]
            .as_object_mut()
            .expect("canonical request object")
            .remove("tools");
        invalid["request"]["input"] = json!([
            {"type":"message", "role":"user", "content":[
                {"type":"input_text", "text":"Inspect the code."}
            ]}
        ]);
        invalid["request"]["tool_choice"] = Value::String("required".to_owned());
        let invalid = serde_json::to_vec(&invalid).expect("invalid request JSON");
        let Err(error) = prepare_anthropic_request(&invalid, "glm-5.2", options()) else {
            panic!("required choice without tools is invalid");
        };
        assert_eq!(error.kind(), AnthropicCodecErrorKind::InvalidRequest);

        let mut unknown_choice: Value =
            serde_json::from_slice(&canonical_request()).expect("canonical request value");
        unknown_choice["request"]["tool_choice"] = Value::String("inspect".to_owned());
        let unknown_choice = serde_json::to_vec(&unknown_choice).expect("unknown tool choice JSON");
        let Err(error) = prepare_anthropic_request(&unknown_choice, "glm-5.2", options()) else {
            panic!("unknown tool choice was accepted");
        };
        assert_eq!(error.kind(), AnthropicCodecErrorKind::InvalidRequest);

        let mut unsupported: Value =
            serde_json::from_slice(&canonical_request()).expect("canonical request value");
        unsupported["request"]["tools"][0]["output_schema"] = json!({"type":"object"});
        let unsupported = serde_json::to_vec(&unsupported).expect("unsupported request JSON");
        let Err(error) = prepare_anthropic_request(&unsupported, "glm-5.2", options()) else {
            panic!("unenforced output schema must be rejected");
        };
        assert_eq!(error.kind(), AnthropicCodecErrorKind::InvalidRequest);
    }

    #[test]
    fn reasoning_effort_is_explicit_on_the_anthropic_wire_and_invalid_values_fail_closed() {
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            let mut request: Value =
                serde_json::from_slice(&canonical_request()).expect("canonical request value");
            request["request"]["reasoning"]["effort"] = Value::String(effort.to_owned());
            let request = serde_json::to_vec(&request).expect("reasoning request JSON");
            let prepared = prepare_anthropic_request(&request, "glm-5.2", options())
                .expect("supported Anthropic effort");
            let body: Value =
                serde_json::from_slice(&prepared.body).expect("Anthropic request JSON");
            assert_eq!(body["thinking"], json!({"type": "adaptive"}));
            assert_eq!(body["output_config"], json!({"effort": effort}));
        }

        let mut request_without_effort: Value =
            serde_json::from_slice(&canonical_request()).expect("canonical request value");
        request_without_effort["request"]["reasoning"] = json!({"summary": "auto"});
        let request_without_effort =
            serde_json::to_vec(&request_without_effort).expect("reasoning request JSON");
        let prepared = prepare_anthropic_request(&request_without_effort, "glm-5.2", options())
            .expect("reasoning request without effort");
        let body: Value = serde_json::from_slice(&prepared.body).expect("Anthropic request JSON");
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());

        for effort in ["", "minimal", "none", "ultra"] {
            let mut invalid: Value =
                serde_json::from_slice(&canonical_request()).expect("canonical request value");
            invalid["request"]["reasoning"]["effort"] = Value::String(effort.to_owned());
            let invalid = serde_json::to_vec(&invalid).expect("invalid reasoning request JSON");
            let Err(error) = prepare_anthropic_request(&invalid, "glm-5.2", options()) else {
                panic!("unsupported Anthropic effort was accepted");
            };
            assert_eq!(error.kind(), AnthropicCodecErrorKind::InvalidRequest);
        }
    }

    #[test]
    fn anthropic_text_thinking_and_tool_stream_maps_usage_and_terminal() {
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"glm-5.2\",\"content\":[],\"usage\":{\"input_tokens\":11,\"cache_read_input_tokens\":2,\"cache_creation_input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"check\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-3\",\"name\":\"read_file\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":11,\"cache_read_input_tokens\":2,\"output_tokens\":7,\"server_tool_use\":{\"web_search_requests\":0},\"service_tier\":\"standard\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let parsed = parse_anthropic_sse(
            body.as_bytes(),
            64 * 1024,
            64,
            &bindings(&[("read_file", ProviderToolKind::Function)]),
            options(),
        )
        .expect("parse Anthropic SSE");
        assert!(parsed.events.iter().any(|event| matches!(
            event,
            ProviderStreamEvent::ReasoningContentDelta { delta, .. } if delta == "check"
        )));
        assert!(parsed.events.iter().any(|event| matches!(
            event,
            ProviderStreamEvent::ToolCallArgumentsDelta { delta, .. }
                if delta == "{\"path\":\"README.md\"}"
        )));
        assert!(matches!(
            parsed.terminal,
            ProviderGatewayTerminal::Completed {
                usage: ProviderTokenUsage {
                    input_tokens: 16,
                    cached_input_tokens: 2,
                    cache_write_input_tokens: 3,
                    output_tokens: 7,
                    reasoning_output_tokens: 0,
                },
                actual_cost_micros: 38,
            }
        ));
    }

    #[test]
    fn anthropic_parallel_function_and_custom_tools_keep_first_seen_identity() {
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-tools\",",
            "\"type\":\"message\",\"role\":\"assistant\",",
            "\"usage\":{\"input_tokens\":2,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":7,",
            "\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-fn\",",
            "\"name\":\"read_file\",\"input\":{}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":3,",
            "\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-custom\",",
            "\"name\":\"apply_patch\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":3,",
            "\"delta\":{\"type\":\"input_json_delta\",",
            "\"partial_json\":\"{\\\"input\\\":\\\"patch\\\"}\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":7,",
            "\"delta\":{\"type\":\"input_json_delta\",",
            "\"partial_json\":\"{\\\"path\\\":\\\"src/lib.rs\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":7}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",",
            "\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},",
            "\"usage\":{\"output_tokens\":4}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let bindings = bindings(&[
            ("apply_patch", ProviderToolKind::Custom),
            ("read_file", ProviderToolKind::Function),
        ]);
        let parsed = parse_anthropic_sse(body.as_bytes(), 64 * 1024, 64, &bindings, options())
            .expect("parse parallel Anthropic tools");
        let starts = parsed
            .events
            .iter()
            .filter_map(|event| match event {
                ProviderStreamEvent::ToolCallStarted {
                    provider_call_id,
                    identity,
                    ..
                } => Some((provider_call_id.as_str(), identity.name(), identity.kind())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            starts,
            vec![
                ("call-fn", "read_file", ProviderToolKind::Function),
                ("call-custom", "apply_patch", ProviderToolKind::Custom),
            ]
        );
        assert!(parsed.events.iter().any(|event| matches!(
            event,
            ProviderStreamEvent::ToolCallArgumentsDelta { provider_call_id, delta, .. }
                if provider_call_id == "call-fn" && delta == "{\"path\":\"src/lib.rs\"}"
        )));
        assert!(parsed.events.iter().any(|event| matches!(
            event,
            ProviderStreamEvent::ToolCallArgumentsDelta { provider_call_id, delta, .. }
                if provider_call_id == "call-custom" && delta == "patch"
        )));
    }

    #[test]
    fn anthropic_usage_requires_initial_counts_and_never_moves_backwards() {
        for value in [
            json!({"output_tokens":0}),
            json!({"input_tokens":1}),
            json!({"input_tokens":-1,"output_tokens":0}),
            json!({"input_tokens":1.5,"output_tokens":0}),
            json!({"input_tokens":MAX_SAFE_INTEGER + 1,"output_tokens":0}),
            json!({"input_tokens":MAX_SAFE_INTEGER,"cache_read_input_tokens":1,"output_tokens":0}),
        ] {
            assert!(anthropic_usage(&value, None).is_err());
        }
        let initial = anthropic_usage(
            &json!({
                "input_tokens": 8,
                "cache_read_input_tokens": 2,
                "cache_creation_input_tokens": 1,
                "output_tokens": 3,
            }),
            None,
        )
        .expect("initial usage");
        let advanced = anthropic_usage(&json!({"output_tokens":7}), Some(initial))
            .expect("monotonic usage delta");
        assert_eq!(advanced.input_tokens, 11);
        assert_eq!(advanced.output_tokens, 7);
        for value in [
            json!({"input_tokens":7,"output_tokens":7}),
            json!({"cache_read_input_tokens":1,"output_tokens":7}),
            json!({"cache_creation_input_tokens":0,"output_tokens":7}),
            json!({"output_tokens":2}),
            json!({"output_tokens":7,"unknown":1}),
        ] {
            assert!(anthropic_usage(&value, Some(initial)).is_err());
        }
    }

    #[test]
    fn duplicate_mismatched_and_open_block_lifecycles_fail_closed() {
        let start = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-1\",",
            "\"type\":\"message\",\"role\":\"assistant\",",
            "\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n"
        );
        let invalid = [
            format!("{start}{start}"),
            format!(
                "{start}event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"x\"}}}}\n\n"
            ),
            format!(
                "{start}event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"thinking_delta\",\"thinking\":\"x\"}}}}\n\n"
            ),
            format!(
                "{start}event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":1}}}}\n\n"
            ),
            "event: ping\ndata: {\"type\":\"message_stop\"}\n\n".to_owned(),
        ];
        for body in invalid {
            let Err(error) = parse_anthropic_sse(
                body.as_bytes(),
                64 * 1024,
                64,
                &AnthropicToolBindings::default(),
                options(),
            ) else {
                panic!("invalid lifecycle must fail closed");
            };
            assert_eq!(error.kind(), AnthropicCodecErrorKind::Protocol);
        }
        let Err(error) = parse_anthropic_sse(
            start.as_bytes(),
            16,
            64,
            &AnthropicToolBindings::default(),
            options(),
        ) else {
            panic!("oversized SSE line must fail closed");
        };
        assert_eq!(error.kind(), AnthropicCodecErrorKind::SizeLimit);
    }

    #[test]
    fn anthropic_stream_unknown_order_limit_error_and_disconnect_fail_closed() {
        let tool_bindings = AnthropicToolBindings::default();
        for body in [
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            "event: unknown\ndata: {\"type\":\"unknown\"}\n\n",
            concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-1\",",
                "\"type\":\"message\",\"role\":\"assistant\",",
                "\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n"
            ),
        ] {
            let Err(error) =
                parse_anthropic_sse(body.as_bytes(), 64 * 1024, 64, &tool_bindings, options())
            else {
                panic!("invalid lifecycle must fail");
            };
            assert_eq!(error.kind(), AnthropicCodecErrorKind::Protocol);
        }

        let too_many = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-1\",",
            "\"type\":\"message\",\"role\":\"assistant\",",
            "\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: ping\n",
            "data: {\"type\":\"ping\"}\n\n"
        );
        let Err(error) =
            parse_anthropic_sse(too_many.as_bytes(), 64 * 1024, 1, &tool_bindings, options())
        else {
            panic!("event count must be bounded");
        };
        assert_eq!(error.kind(), AnthropicCodecErrorKind::SizeLimit);

        for (upstream, expected) in [
            (
                "authentication_error",
                ProviderStreamFailureKind::Authentication,
            ),
            (
                "permission_error",
                ProviderStreamFailureKind::Authentication,
            ),
            (
                "invalid_request_error",
                ProviderStreamFailureKind::InvalidRequest,
            ),
            ("not_found_error", ProviderStreamFailureKind::InvalidRequest),
            ("rate_limit_error", ProviderStreamFailureKind::RateLimit),
            ("overloaded_error", ProviderStreamFailureKind::Server),
            ("api_error", ProviderStreamFailureKind::Server),
            ("future_error", ProviderStreamFailureKind::Unknown),
        ] {
            let body = format!(
                "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"{upstream}\",\"message\":\"safe\"}}}}\n\n"
            );
            let parsed =
                parse_anthropic_sse(body.as_bytes(), 64 * 1024, 64, &tool_bindings, options())
                    .expect("typed upstream error terminal");
            assert!(matches!(
                parsed.events.as_slice(),
                [ProviderStreamEvent::Failed(failure)] if failure.kind() == expected
            ));
            assert!(matches!(
                parsed.terminal,
                ProviderGatewayTerminal::Failed { .. }
            ));
        }
    }
}

#[cfg(test)]
#[path = "provider_anthropic_namespace_tests.rs"]
mod provider_anthropic_namespace_tests;

#[cfg(test)]
#[path = "provider_anthropic_namespace_safety_tests.rs"]
mod provider_anthropic_namespace_safety_tests;
