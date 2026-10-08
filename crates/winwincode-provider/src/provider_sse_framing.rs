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

struct ParseOutcome {
    frames: Vec<SseFrame>,
    error: Option<SseFramingError>,
}

pub(crate) fn parse(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
) -> Result<Vec<SseFrame>, SseFramingError> {
    let outcome = parse_frames(bytes, max_event_bytes, max_events);
    match outcome.error {
        Some(error) => Err(error),
        None => Ok(outcome.frames),
    }
}

/// Retains completed bounded frames for private accounting after a later size failure.
/// UTF-8 validity remains required; a valid prefix never establishes stream success.
pub(crate) fn parse_prefix(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
) -> Result<Vec<SseFrame>, SseFramingError> {
    let outcome = parse_frames(bytes, max_event_bytes, max_events);
    match outcome.error {
        Some(SseFramingError::Utf8) => Err(SseFramingError::Utf8),
        Some(SseFramingError::SizeLimit) | None => Ok(outcome.frames),
    }
}

fn parse_frames(bytes: &[u8], max_event_bytes: usize, max_events: usize) -> ParseOutcome {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return ParseOutcome {
            frames: Vec::new(),
            error: Some(SseFramingError::Utf8),
        };
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut outcome = ParseOutcome {
        frames: Vec::new(),
        error: None,
    };
    let mut event = None;
    let mut data = String::new();
    let mut data_seen = false;
    for line in normalized.split('\n') {
        if line.len() > max_event_bytes {
            outcome.error = Some(SseFramingError::SizeLimit);
            return outcome;
        }
        if line.is_empty() {
            if let Err(error) = flush(
                &mut outcome.frames,
                &mut event,
                &mut data,
                &mut data_seen,
                max_events,
            ) {
                outcome.error = Some(error);
                return outcome;
            }
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
                    outcome.error = Some(SseFramingError::SizeLimit);
                    return outcome;
                }
            }
            // No transport reconnect is performed from model-supplied fields.
            // id/retry/extension fields do not change JSON data or authority.
            _ => {}
        }
    }
    // Preserve existing codec EOF handling; terminal semantics remain strict.
    outcome.error = flush(
        &mut outcome.frames,
        &mut event,
        &mut data,
        &mut data_seen,
        max_events,
    )
    .err();
    outcome
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

    #[test]
    fn accounting_prefix_keeps_completed_frames_when_a_later_limit_fails() {
        let prefix = b"data: one\n\ndata: two\n\n";
        for suffix in [
            "data: ".to_owned() + &"x".repeat(33) + "\n\n",
            "data: ".to_owned() + &"x".repeat(17) + "\ndata: " + &"x".repeat(17) + "\n\n",
            "data: three\n\ndata: four\n\n".into(),
        ] {
            let mut bytes = prefix.to_vec();
            bytes.extend_from_slice(suffix.as_bytes());
            let frames = parse_prefix(&bytes, 32, 2).unwrap();
            assert_eq!(
                frames
                    .into_iter()
                    .map(|frame| frame.data)
                    .collect::<Vec<_>>(),
                vec!["one", "two"]
            );
            assert!(matches!(
                parse(&bytes, 32, 2),
                Err(SseFramingError::SizeLimit)
            ));
        }
        assert!(parse_prefix(prefix, 32, 0).unwrap().is_empty());
        assert!(parse_prefix(prefix, 0, 2).unwrap().is_empty());
    }

    #[test]
    fn accounting_prefix_preserves_the_utf8_boundary() {
        for suffix in [vec![0xff], vec![0xe4, 0xb8]] {
            let mut bytes = b"data: valid\n\n".to_vec();
            bytes.extend_from_slice(&suffix);
            assert!(matches!(
                parse_prefix(&bytes, 32, 2),
                Err(SseFramingError::Utf8)
            ));
            assert!(matches!(parse(&bytes, 32, 2), Err(SseFramingError::Utf8)));
        }
    }
}
