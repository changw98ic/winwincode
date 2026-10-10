// SPDX-License-Identifier: Apache-2.0

//! Validated inline media shared by request normalization and protocol projection.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};

use crate::provider_anthropic::{AnthropicCodecError, exact_keys, object, required, string};

pub(crate) fn normalize_audio(value: &Map<String, Value>) -> Result<Value, AnthropicCodecError> {
    exact_keys(value, &["type", "audio_url"])?;
    let (media_type, data) = string(value, "audio_url")?
        .strip_prefix("data:")
        .and_then(|url| url.split_once(";base64,"))
        .ok_or_else(AnthropicCodecError::invalid_request)?;
    if data.is_empty() || data.len() > 16 * 1024 * 1024 {
        return Err(AnthropicCodecError::size_limit());
    }
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| AnthropicCodecError::invalid_request())?;
    let format = match media_type {
        "audio/wav" | "audio/x-wav"
            if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") =>
        {
            "wav"
        }
        "audio/mpeg" | "audio/mp3"
            if bytes.starts_with(b"ID3")
                || bytes.first() == Some(&0xff)
                    && bytes.get(1).is_some_and(|byte| byte & 0xe0 == 0xe0) =>
        {
            "mp3"
        }
        _ => {
            return Err(AnthropicCodecError::invalid_request().with_diagnostic(
                "unsupported_media",
                "input_audio",
                "$.audio_url",
            ));
        }
    };
    Ok(json!({"type":"audio", "source":{"format":format,"data":data}}))
}

pub(crate) fn openai_audio(value: &Map<String, Value>) -> Result<Value, AnthropicCodecError> {
    exact_keys(value, &["type", "source"])?;
    let source = object(required(value, "source")?)?;
    exact_keys(source, &["format", "data"])?;
    Ok(json!({"type":"input_audio", "input_audio":{
        "format":string(source,"format")?, "data":string(source,"data")?
    }}))
}
