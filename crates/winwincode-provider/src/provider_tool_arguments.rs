// SPDX-License-Identifier: Apache-2.0

//! Admission of JSON wrappers used to represent custom tools on function-only
//! provider protocols. A model's invalid wrapper is still a function call;
//! it must reach Core's payload validation without becoming executable input
//! or a retryable failure of the entire model response.

use serde_json::{Map, Value};

use crate::provider_anthropic::{AnthropicCodecError, validate_text};
use crate::{ProviderStreamEvent, ProviderToolIdentity, ProviderToolKind};

pub(crate) fn translated_custom_arguments(
    input: &Map<String, Value>,
    index: u32,
    call_id: &str,
    events: &mut [ProviderStreamEvent],
) -> Result<String, AnthropicCodecError> {
    if input.len() == 1
        && let Some(raw_input) = input.get("input").and_then(Value::as_str)
    {
        validate_text(raw_input)?;
        return Ok(raw_input.to_owned());
    }

    // These adapters buffer the entire response before canonical conversion.
    // Change the original start, so added/done items retain the same payload
    // kind and identity. No field is guessed, dropped, or coerced into code.
    let identity = events
        .iter_mut()
        .find_map(|event| match event {
            ProviderStreamEvent::ToolCallStarted {
                index: started_index,
                provider_call_id,
                identity,
            } if *started_index == index && provider_call_id == call_id => Some(identity),
            _ => None,
        })
        .ok_or_else(AnthropicCodecError::protocol)?;
    if identity.kind() != ProviderToolKind::Custom {
        return Err(AnthropicCodecError::protocol());
    }
    *identity = ProviderToolIdentity::try_new(
        ProviderToolKind::Function,
        identity.name().to_owned(),
        identity.namespace().map(str::to_owned),
    )
    .map_err(|_| AnthropicCodecError::protocol())?;
    serde_json::to_string(input).map_err(|_| AnthropicCodecError::protocol())
}

#[cfg(test)]
#[path = "provider_tool_arguments_tests.rs"]
mod tests;
