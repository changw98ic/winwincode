// SPDX-License-Identifier: Apache-2.0

//! Selects a single model-produced JSON object without changing its bytes.
//! Wire messages still use their strict parsers. Callers must validate JSON,
//! duplicate fields, schema, identities and evidence after this selection.

/// Representation of a selected model result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelResultRepresentation {
    /// A plain JSON object, possibly with surrounding whitespace or a BOM.
    Object,
    /// A single JSON object surrounded by explanatory text.
    ProseObject,
    /// A single JSON object in a Markdown JSON code fence.
    JsonFence,
}

/// Original object bytes and their representation; no values are repaired.
#[derive(Clone, Copy, Debug)]
pub struct NormalizedModelObject<'a> {
    pub json: &'a str,
    pub representation: ModelResultRepresentation,
}

/// Selection failed because the input was oversized, incomplete or ambiguous.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelResultNormalizationError;

/// Selects the unique complete object within the caller's raw byte limit.
///
/// Accepts a BOM, LF/CRLF, ordinary prose reference brackets and Markdown
/// backtick/tilde fences of at least three characters. Fences may be indented
/// by at most three spaces and carry an empty or case-insensitive JSON label.
///
/// # Errors
/// Rejects arrays containing the result, multiple objects or code fences,
/// malformed delimiters, nesting deeper than 64, and incomplete fences.
pub fn normalize_model_object(
    raw: &str,
    max_bytes: usize,
) -> Result<NormalizedModelObject<'_>, ModelResultNormalizationError> {
    let fail = ModelResultNormalizationError;
    if raw.len() > max_bytes {
        return Err(fail);
    }
    let message = raw
        .trim()
        .strip_prefix('\u{feff}')
        .unwrap_or(raw.trim())
        .trim();
    let (start, end) = unique_object(message)?;
    let prefix = &message[..start];
    let suffix = &message[end..];
    let opening: Vec<_> = prefix.lines().filter_map(fence).collect();
    let closing: Vec<_> = suffix.lines().filter_map(fence).collect();
    let representation = match (opening.as_slice(), closing.as_slice()) {
        ([], []) if prefix.trim().is_empty() && suffix.trim().is_empty() => {
            ModelResultRepresentation::Object
        }
        ([], []) => ModelResultRepresentation::ProseObject,
        ([open], [close])
            if open.marker == close.marker
                && close.length >= open.length
                && (open.info.is_empty() || open.info.eq_ignore_ascii_case("json"))
                && close.info.is_empty()
                && prefix
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .and_then(fence)
                    == Some(*open)
                && suffix
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .and_then(fence)
                    == Some(*close) =>
        {
            ModelResultRepresentation::JsonFence
        }
        _ => return Err(fail),
    };
    Ok(NormalizedModelObject {
        json: &message[start..end],
        representation,
    })
}

#[derive(Clone, Copy, PartialEq)]
struct Fence<'a> {
    marker: u8,
    length: usize,
    info: &'a str,
}

fn fence(line: &str) -> Option<Fence<'_>> {
    let stripped = line.trim_start_matches(' ');
    if line.len() - stripped.len() > 3 {
        return None;
    }
    let marker = *stripped.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let length = stripped.bytes().take_while(|byte| *byte == marker).count();
    (length >= 3).then(|| Fence {
        marker,
        length,
        info: stripped[length..].trim(),
    })
}

fn unique_object(message: &str) -> Result<(usize, usize), ModelResultNormalizationError> {
    let fail = ModelResultNormalizationError;
    let mut stack = Vec::new();
    let mut prose_brackets = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    let mut start = None;
    let mut result = None;
    for (index, byte) in message.bytes().enumerate() {
        if !stack.is_empty() && quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        if stack.is_empty() {
            match byte {
                b'[' => {
                    prose_brackets += 1;
                    continue;
                }
                b']' => {
                    prose_brackets = prose_brackets.saturating_sub(1);
                    continue;
                }
                b'{' if prose_brackets > 0 || result.is_some() => return Err(fail),
                _ => {}
            }
        }
        match byte {
            b'"' if !stack.is_empty() => quoted = true,
            b'{' | b'[' => {
                if stack.is_empty() {
                    start = Some(index);
                }
                if stack.len() == 64 {
                    return Err(fail);
                }
                stack.push(byte);
            }
            b'}' | b']' => {
                if stack.pop() != Some(if byte == b'}' { b'{' } else { b'[' }) {
                    return Err(fail);
                }
                if stack.is_empty() {
                    result = Some((start.ok_or(fail)?, index + 1));
                }
            }
            _ => {}
        }
    }
    if !stack.is_empty() || quoted {
        return Err(fail);
    }
    result.ok_or(fail)
}
