// SPDX-License-Identifier: Apache-2.0

//! Cross-implementation conformance for `winwincode-export/v1`.
//!
//! The published vectors are executed by this crate and by the non-Rust strict gate
//! (`schema/winwincode-export/v1/validate.js`). Both implementations must return the same
//! verdict for every vector and, for a rejection, the same category.

use std::collections::BTreeSet;

use serde::Deserialize;

use winwincode_data_export::{WinWinCodeExport, WinWinCodeExportError};

const VECTORS: &str = include_str!("../../../schema/winwincode-export/v1/conformance-vectors.json");
const PUBLISHED_FIXTURE: &[u8] =
    include_bytes!("../../../schema/winwincode-export/v1/winwincode-export.example.json.bytes");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ConformanceVectors {
    schema: String,
    description: String,
    vectors: Vec<ConformanceVector>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ConformanceVector {
    id: String,
    expectation: String,
    category: Option<String>,
    bytes_hex: String,
}

fn published_vectors() -> ConformanceVectors {
    serde_json::from_str(VECTORS).expect("published conformance vectors deserialize")
}

fn rejection_category(error: WinWinCodeExportError) -> &'static str {
    match error {
        WinWinCodeExportError::TooLarge => "too-large",
        WinWinCodeExportError::InvalidJson => "invalid-json",
        WinWinCodeExportError::UnsupportedFormat => "unsupported-format",
        WinWinCodeExportError::InvalidExportId => "invalid-export-id",
        WinWinCodeExportError::InvalidContent => "invalid-content",
        WinWinCodeExportError::LocalPathNotAllowed => "local-path-not-allowed",
        WinWinCodeExportError::DuplicateSourceIdentifier => "duplicate-source-identifier",
        WinWinCodeExportError::UnknownSourceOrganization => "unknown-source-organization",
        WinWinCodeExportError::DigestMismatch => "digest-mismatch",
        WinWinCodeExportError::NonCanonical => "non-canonical",
    }
}

fn vector_bytes(vector: &ConformanceVector) -> Vec<u8> {
    let digits = vector.bytes_hex.as_bytes();
    assert_eq!(
        digits.len() % 2,
        0,
        "{}: even hexadecimal length",
        vector.id
    );
    digits
        .chunks_exact(2)
        .map(|pair| (hexadecimal_value(pair[0]) << 4) | hexadecimal_value(pair[1]))
        .collect()
}

fn hexadecimal_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        other => panic!("invalid hexadecimal digit {other}"),
    }
}

#[test]
fn published_vectors_are_broad_and_categorized() {
    let vectors = published_vectors();
    assert_eq!(vectors.schema, "winwincode-export-conformance-vectors/v1");
    assert!(
        vectors.description.contains("non-Rust strict gate"),
        "vectors must document that both implementations execute them",
    );
    assert!(vectors.vectors.len() >= 40, "expected a broad vector set");
    let mut ids = BTreeSet::new();
    let mut rejected_categories = BTreeSet::new();
    for vector in &vectors.vectors {
        assert!(
            ids.insert(vector.id.as_str()),
            "duplicate vector id {}",
            vector.id
        );
        match vector.expectation.as_str() {
            "accept" => assert_eq!(vector.category, None, "{}", vector.id),
            "reject" => {
                let category = vector.category.as_deref().expect("rejection category");
                rejected_categories.insert(category);
            }
            other => panic!("{}: unknown expectation {other}", vector.id),
        }
    }
    assert!(rejected_categories.contains("duplicate-source-identifier"));
    assert!(rejected_categories.contains("unknown-source-organization"));
    assert!(rejected_categories.contains("non-canonical"));
    assert!(rejected_categories.contains("digest-mismatch"));
}

#[test]
fn published_fixture_is_one_of_the_published_vectors() {
    let vectors = published_vectors();
    let fixture = vectors
        .vectors
        .iter()
        .find(|vector| vector.id == "published-fixture-is-canonical")
        .expect("the published fixture is a vector");
    assert_eq!(fixture.expectation, "accept");
    assert_eq!(vector_bytes(fixture), PUBLISHED_FIXTURE);
}

#[test]
fn rust_and_the_non_rust_gate_must_agree_on_every_vector() {
    let vectors = published_vectors();
    for vector in &vectors.vectors {
        let bytes = vector_bytes(vector);
        match WinWinCodeExport::decode_canonical(&bytes) {
            Ok(export) => {
                assert_eq!(vector.expectation, "accept", "{}: {:?}", vector.id, export);
                assert_eq!(
                    export.encode_canonical().expect("accepted export encodes"),
                    bytes,
                    "{} must reproduce the published bytes",
                    vector.id,
                );
            }
            Err(error) => {
                assert_eq!(vector.expectation, "reject", "{}: {error}", vector.id);
                assert_eq!(
                    rejection_category(error),
                    vector.category.as_deref().unwrap_or_default(),
                    "{} must match the published category",
                    vector.id,
                );
            }
        }
    }
}
