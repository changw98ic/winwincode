// SPDX-License-Identifier: Apache-2.0

//! Serde wiring shared by the `ClientControlPort` contract types.

/// `serde(with)` helpers for occupancy fencing tokens.
///
/// The authoritative schema encodes a fencing token as a decimal string
/// matching `^[1-9][0-9]{0,19}$` (`OccupancyFencingToken`, identical to the
/// `ExecutionPort` `FencingToken` definition) to preserve 64-bit precision,
/// while the in-memory type stays [`u64`].
pub mod fencing_token {
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serializer;
    use serde::de::Error as _;
    use serde::ser::Error as _;

    /// Parses a wire-encoded occupancy fencing token.
    ///
    /// The text must match `^[1-9][0-9]{0,19}$` and fit a [`u64`].
    ///
    /// # Errors
    ///
    /// Returns the reason the text is not a valid wire-encoded token.
    pub fn parse_token(text: &str) -> Result<u64, String> {
        let bytes = text.as_bytes();
        let shape_ok = matches!(bytes.first(), Some(&first) if matches!(first, b'1'..=b'9'))
            && bytes.len() <= 20
            && bytes.iter().skip(1).all(u8::is_ascii_digit);
        if !shape_ok {
            return Err(format!(
                "occupancy fencing token must match ^[1-9][0-9]{{0,19}}$, got {text:?}"
            ));
        }
        text.parse::<u64>()
            .map_err(|_| format!("occupancy fencing token exceeds the u64 range: {text:?}"))
    }

    /// Serializes a fencing token as its decimal string form.
    ///
    /// # Errors
    ///
    /// Fails when the token is `0`; the wire pattern starts at `1`.
    pub fn serialize<S>(token: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if *token == 0 {
            return Err(S::Error::custom(
                "occupancy fencing token must be at least 1",
            ));
        }
        serializer.collect_str(token)
    }

    /// Deserializes a fencing token from its decimal string form.
    ///
    /// # Errors
    ///
    /// Fails when the text does not match the wire pattern or exceeds the
    /// [`u64`] range.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        parse_token(&text).map_err(D::Error::custom)
    }

    #[cfg(test)]
    mod tests {
        use super::parse_token;

        #[test]
        fn accepts_valid_wire_encodings() {
            assert_eq!(parse_token("1"), Ok(1));
            assert_eq!(parse_token("7"), Ok(7));
            assert_eq!(
                parse_token("18446744073709551615"),
                Ok(u64::MAX),
                "the full u64 range stays representable"
            );
        }

        #[test]
        fn rejects_invalid_wire_encodings() {
            for bad in [
                "",
                "0",
                "01",
                "-1",
                " 1",
                "1 ",
                "1a",
                "a1",
                "1234567890123456789a",
                &"9".repeat(21),
                "18446744073709551616",
            ] {
                assert!(
                    parse_token(bad).is_err(),
                    "{bad:?} must not parse as a fencing token"
                );
            }
        }
    }
}
