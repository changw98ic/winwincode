use crate::ChatAttachment;
use base64::{Engine as _, engine::general_purpose::STANDARD};

pub fn validate_chat_attachments(attachments: &[ChatAttachment]) -> Result<(), &'static str> {
    if attachments.len() > 4 {
        return Err("At most four attachments are allowed");
    }
    let mut total = 0;
    for item in attachments {
        if item.name.trim().is_empty()
            || item.name.chars().count() > 255
            || item.name.chars().any(char::is_control)
        {
            return Err("Invalid attachment name");
        }
        if item.content.is_empty() || item.content.len() > 174_764 {
            return Err("Invalid attachment size");
        }
        if item.media_type == "text/plain" {
            if item.content.len() > 32_768
                || item
                    .content
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
            {
                return Err("Text attachments must be UTF-8 text up to 32 KiB");
            }
            total += item.content.len();
        } else {
            let bytes = STANDARD
                .decode(&item.content)
                .map_err(|_| "Invalid attachment encoding")?;
            let valid = match item.media_type.as_str() {
                "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                "image/jpeg" => {
                    bytes.starts_with(&[0xff, 0xd8, 0xff]) && bytes.ends_with(&[0xff, 0xd9])
                }
                "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
                _ => false,
            };
            if !valid {
                return Err("Invalid image attachment");
            }
            total += bytes.len();
        }
        if total > 131_072 {
            return Err("Attachments exceed 128 KiB per message");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_types_bytes_and_limits() {
        let text = ChatAttachment {
            name: "code.ts".into(),
            media_type: "text/plain".into(),
            content: "const n = 42;".into(),
        };
        assert!(validate_chat_attachments(std::slice::from_ref(&text)).is_ok());
        for (media_type, content) in [
            ("image/svg+xml", "<svg/>"),
            ("image/png", "YmFk"),
            ("text/plain", "a\0b"),
        ] {
            assert!(
                validate_chat_attachments(&[ChatAttachment {
                    media_type: media_type.into(),
                    content: content.into(),
                    ..text.clone()
                }])
                .is_err()
            );
        }
        assert!(validate_chat_attachments(&vec![text; 5]).is_err());
    }
}
