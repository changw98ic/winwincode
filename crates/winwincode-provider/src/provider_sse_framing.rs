// SPDX-License-Identifier: Apache-2.0

//! Shared SSE representation parser. Metadata never grants model authority;
//! each codec validates the unmodified data and its own terminal ordering.

#[derive(Debug)]
pub(crate) struct SseFrame {
    pub(crate) event: Option<String>,
    pub(crate) data: String,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SseFramingError {
    Utf8,
    SizeLimit,
}

pub(crate) fn parse(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
) -> Result<Vec<SseFrame>, SseFramingError> {
    let text = std::str::from_utf8(bytes).map_err(|_| SseFramingError::Utf8)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut frames = Vec::new();
    let mut event = None;
    let mut data = String::new();
    let mut data_seen = false;
    for line in normalized.split('\n') {
        if line.len() > max_event_bytes {
            return Err(SseFramingError::SizeLimit);
        }
        if line.is_empty() {
            flush(
                &mut frames,
                &mut event,
                &mut data,
                &mut data_seen,
                max_events,
            )?;
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => {
                if data_seen {
                    data.push('\n');
                }
                data_seen = true;
                data.push_str(value);
                if data.len() > max_event_bytes {
                    return Err(SseFramingError::SizeLimit);
                }
            }
            // No transport reconnect is performed from model-supplied fields.
            // id/retry/extension fields do not change JSON data or authority.
            _ => {}
        }
    }
    // Preserve existing codec EOF handling; terminal semantics remain strict.
    flush(
        &mut frames,
        &mut event,
        &mut data,
        &mut data_seen,
        max_events,
    )?;
    Ok(frames)
}

fn flush(
    frames: &mut Vec<SseFrame>,
    event: &mut Option<String>,
    data: &mut String,
    seen: &mut bool,
    limit: usize,
) -> Result<(), SseFramingError> {
    if *seen {
        if frames.len() >= limit {
            return Err(SseFramingError::SizeLimit);
        }
        frames.push(SseFrame {
            event: event.take(),
            data: std::mem::take(data),
        });
    } else {
        *event = None;
    }
    *seen = false;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_preserves_data_and_bounds_while_ignoring_metadata() {
        let wire = "\u{feff}id: a\0b\rretry: 100\rdata:\rdata: {\"a\":1}\r\r";
        let frames = parse(wire.as_bytes(), 64, 1).unwrap();
        assert_eq!(frames[0].data, "\n{\"a\":1}");
        assert!(parse(wire.as_bytes(), 3, 1).is_err());
        assert!(parse(wire.as_bytes(), 64, 0).is_err());
        assert!(parse(&[0xff], 64, 1).is_err());
    }
}
