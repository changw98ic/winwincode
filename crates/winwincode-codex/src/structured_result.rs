// SPDX-License-Identifier: Apache-2.0

//! Representation normalization for typed model results. Evidence and authority
//! validation remain with the caller; this module never repairs JSON values.

use serde::de::DeserializeOwned;
use sha2::{Digest as _, Sha256};

const MAX_RESULT_BYTES: usize = 1024 * 1024;
#[cfg(test)]
const MAX_DEPTH: usize = 64;

pub(crate) fn decode<T: DeserializeOwned>(raw: &str) -> Result<T, ()> {
    if raw.len() > MAX_RESULT_BYTES {
        return Err(());
    }
    let normalized = winwincode_execution_port::model_result_normalizer::normalize_model_object(
        raw,
        MAX_RESULT_BYTES,
    )
    .map_err(|_| ())?;
    // Direct typed decoding retains duplicate and unknown field rejection.
    let result = serde_json::from_str(normalized.json).map_err(|_| ())?;
    if normalized.json != raw {
        eprintln!(
            "structured_result_normalized kind={:?} raw_sha256={:x}",
            normalized.representation,
            Sha256::digest(raw.as_bytes())
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct ResultObject {
        value: String,
    }

    #[test]
    fn normalization_preserves_escaped_strings_and_rejects_ambiguity() {
        let json = r#"{"value":"a { brace } \\\" and ``` inside"}"#;
        let expected: ResultObject = serde_json::from_str(json).unwrap();
        for message in [
            json.to_owned(),
            format!("Result:\n{json}\nDone."),
            format!("Result:\n```json\r\n{json}\r\n```\r\nDone."),
        ] {
            assert_eq!(decode::<ResultObject>(&message).unwrap(), expected);
        }
        for message in [
            r#"{"value":"first","value":"second"}"#.to_owned(),
            format!("{json} {{}}"),
            format!("[{json}]"),
            format!("```json\n{json}\n```\n```text\nextra\n```"),
            "{\"value\":\"truncated".to_owned(),
            "{".repeat(MAX_DEPTH + 1),
            " ".repeat(MAX_RESULT_BYTES + 1),
        ] {
            assert!(decode::<ResultObject>(&message).is_err());
        }
    }

    #[test]
    fn markdown_representation_variants_preserve_the_same_result() {
        for message in [
            "\u{feff}\r\n  ```JSON  \r\n{\"value\":\"ok\"}\r\n  ```  \r\n",
            "Result [1]:\n{\"value\":\"ok\"}\nSee [notes].",
            "~~~json\n{\"value\":\"ok\"}\n~~~",
            "````json\n{\"value\":\"ok\"}\n````",
        ] {
            assert_eq!(
                decode::<ResultObject>(message).unwrap().value,
                "ok",
                "{message}"
            );
        }
    }
}
