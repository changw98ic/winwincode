// SPDX-License-Identifier: Apache-2.0

//! Database-neutral `winwincode-export/v1` contract.
//!
//! [`WinWinCodeExport::try_new`] canonicalizes records and seals the payload. The same value always
//! produces the same compact UTF-8 JSON bytes, regardless of the input record order.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const WINWINCODE_EXPORT_FORMAT: &str = "winwincode-export/v1";
pub const MAX_WINWINCODE_EXPORT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_WINWINCODE_EXPORT_ORGANIZATIONS: usize = 10_000;
pub const MAX_WINWINCODE_EXPORT_PROJECTS: usize = 100_000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WinWinCodeExportOrganization {
    pub source_organization_id: String,
    pub slug: String,
    pub display_name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WinWinCodeExportProject {
    pub source_project_id: String,
    pub source_organization_id: String,
    pub slug: String,
    pub display_name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WinWinCodeExportContent {
    pub profile_display_name: String,
    pub organizations: Vec<WinWinCodeExportOrganization>,
    pub projects: Vec<WinWinCodeExportProject>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WinWinCodeExportCounts {
    pub organizations: usize,
    pub projects: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WinWinCodeExport {
    format: String,
    export_id: String,
    content_sha256: String,
    content: WinWinCodeExportContent,
}

impl WinWinCodeExport {
    /// Builds the only current export shape and canonicalizes record order before hashing it.
    ///
    /// # Errors
    ///
    /// Rejects invalid IDs, duplicate records, missing organization references, unsafe text, or a
    /// document larger than [`MAX_WINWINCODE_EXPORT_BYTES`].
    pub fn try_new(
        export_id: impl Into<String>,
        mut content: WinWinCodeExportContent,
    ) -> Result<Self, WinWinCodeExportError> {
        let export_id = export_id.into();
        normalize_content(&mut content);
        validate_export_id(&export_id)?;
        validate_content(&content)?;
        let content_sha256 = content_digest(&export_id, &content);
        let export = Self {
            format: WINWINCODE_EXPORT_FORMAT.to_owned(),
            export_id,
            content_sha256,
            content,
        };
        encode_bounded(&export)?;
        Ok(export)
    }

    /// Decodes only the canonical compact JSON representation.
    ///
    /// # Errors
    ///
    /// Rejects oversized, malformed, reordered, altered, or unsupported documents.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, WinWinCodeExportError> {
        if bytes.len() > MAX_WINWINCODE_EXPORT_BYTES {
            return Err(WinWinCodeExportError::TooLarge);
        }
        let export: Self =
            serde_json::from_slice(bytes).map_err(|_| WinWinCodeExportError::InvalidJson)?;
        export.validate()?;
        if export.encode_canonical()? != bytes {
            return Err(WinWinCodeExportError::NonCanonical);
        }
        Ok(export)
    }

    /// Returns the canonical compact UTF-8 JSON bytes.
    ///
    /// # Errors
    ///
    /// Rejects an invalid in-memory value or a document outside the size limit.
    pub fn encode_canonical(&self) -> Result<Vec<u8>, WinWinCodeExportError> {
        self.validate()?;
        encode_bounded(self)
    }

    #[must_use]
    pub fn export_id(&self) -> &str {
        &self.export_id
    }

    #[must_use]
    pub fn content_sha256(&self) -> &str {
        &self.content_sha256
    }

    #[must_use]
    pub const fn content(&self) -> &WinWinCodeExportContent {
        &self.content
    }

    #[must_use]
    pub fn domain_counts(&self) -> WinWinCodeExportCounts {
        WinWinCodeExportCounts {
            organizations: self.content.organizations.len(),
            projects: self.content.projects.len(),
        }
    }

    fn validate(&self) -> Result<(), WinWinCodeExportError> {
        if self.format != WINWINCODE_EXPORT_FORMAT {
            return Err(WinWinCodeExportError::UnsupportedFormat);
        }
        validate_export_id(&self.export_id)?;
        validate_content(&self.content)?;
        let mut normalized = self.content.clone();
        normalize_content(&mut normalized);
        if normalized != self.content {
            return Err(WinWinCodeExportError::NonCanonical);
        }
        if !valid_sha256(&self.content_sha256)
            || self.content_sha256 != content_digest(&self.export_id, &self.content)
        {
            return Err(WinWinCodeExportError::DigestMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WinWinCodeExportError {
    TooLarge,
    InvalidJson,
    UnsupportedFormat,
    InvalidExportId,
    InvalidContent,
    LocalPathNotAllowed,
    DuplicateSourceIdentifier,
    UnknownSourceOrganization,
    DigestMismatch,
    NonCanonical,
}

impl Display for WinWinCodeExportError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::TooLarge => "WinWinCode export exceeds its size limit",
            Self::InvalidJson => "WinWinCode export JSON was rejected",
            Self::UnsupportedFormat => "WinWinCode export format is unsupported",
            Self::InvalidExportId => "WinWinCode export ID is invalid",
            Self::InvalidContent => "WinWinCode export content was rejected",
            Self::LocalPathNotAllowed => "WinWinCode export contains a local absolute path",
            Self::DuplicateSourceIdentifier => {
                "WinWinCode export contains a duplicate source identifier"
            }
            Self::UnknownSourceOrganization => {
                "WinWinCode export project references an unknown organization"
            }
            Self::DigestMismatch => "WinWinCode export digest does not match its content",
            Self::NonCanonical => "WinWinCode export bytes or record order are not canonical",
        };
        formatter.write_str(message)
    }
}

impl Error for WinWinCodeExportError {}

fn normalize_content(content: &mut WinWinCodeExportContent) {
    content.organizations.sort_by(|left, right| {
        left.source_organization_id
            .cmp(&right.source_organization_id)
    });
    content.projects.sort_by(|left, right| {
        (&left.source_organization_id, &left.source_project_id)
            .cmp(&(&right.source_organization_id, &right.source_project_id))
    });
}

fn validate_content(content: &WinWinCodeExportContent) -> Result<(), WinWinCodeExportError> {
    validate_display_name(&content.profile_display_name)?;
    if content.organizations.len() > MAX_WINWINCODE_EXPORT_ORGANIZATIONS
        || content.projects.len() > MAX_WINWINCODE_EXPORT_PROJECTS
    {
        return Err(WinWinCodeExportError::InvalidContent);
    }
    let mut organization_ids = BTreeSet::new();
    let mut organization_slugs = BTreeSet::new();
    for organization in &content.organizations {
        validate_source_id(&organization.source_organization_id)?;
        validate_slug(&organization.slug)?;
        validate_display_name(&organization.display_name)?;
        if !organization_ids.insert(organization.source_organization_id.as_str())
            || !organization_slugs.insert(organization.slug.as_str())
        {
            return Err(WinWinCodeExportError::DuplicateSourceIdentifier);
        }
    }
    let mut projects = BTreeMap::new();
    let mut project_slugs = BTreeSet::new();
    for project in &content.projects {
        validate_source_id(&project.source_project_id)?;
        validate_source_id(&project.source_organization_id)?;
        validate_slug(&project.slug)?;
        validate_display_name(&project.display_name)?;
        if !organization_ids.contains(project.source_organization_id.as_str()) {
            return Err(WinWinCodeExportError::UnknownSourceOrganization);
        }
        if projects
            .insert(
                project.source_project_id.as_str(),
                project.source_organization_id.as_str(),
            )
            .is_some()
            || !project_slugs.insert((
                project.source_organization_id.as_str(),
                project.slug.as_str(),
            ))
        {
            return Err(WinWinCodeExportError::DuplicateSourceIdentifier);
        }
    }
    Ok(())
}

fn validate_export_id(value: &str) -> Result<(), WinWinCodeExportError> {
    if value.is_empty()
        || value.len() > 128
        || value.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        })
    {
        return Err(WinWinCodeExportError::InvalidExportId);
    }
    Ok(())
}

fn validate_source_id(value: &str) -> Result<(), WinWinCodeExportError> {
    validate_text(value, 256)
}

fn validate_display_name(value: &str) -> Result<(), WinWinCodeExportError> {
    validate_text(value, 256)
}

fn validate_text(value: &str, maximum: usize) -> Result<(), WinWinCodeExportError> {
    if value.is_empty()
        || value.chars().count() > maximum
        || value.starts_with(is_contract_whitespace)
        || value.ends_with(is_contract_whitespace)
        || value.chars().any(char::is_control)
    {
        return Err(WinWinCodeExportError::InvalidContent);
    }
    if looks_like_absolute_path(value) {
        return Err(WinWinCodeExportError::LocalPathNotAllowed);
    }
    Ok(())
}

fn is_contract_whitespace(character: char) -> bool {
    // ECMA-262 `\s`, used by the JSON Schema pattern, differs from Rust's Unicode whitespace
    // set only by U+0085 and U+FEFF. U+0085 is already rejected as a control character.
    character.is_whitespace() || character == '\u{feff}'
}

fn looks_like_absolute_path(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("~/")
        || value.starts_with("file://")
        || value.starts_with("\\\\")
        || (value.as_bytes().get(1) == Some(&b':')
            && value
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic)
            && value
                .as_bytes()
                .get(2)
                .is_some_and(|byte| matches!(byte, b'/' | b'\\')))
}

fn validate_slug(value: &str) -> Result<(), WinWinCodeExportError> {
    if value.is_empty()
        || value.len() > 128
        || value.starts_with('-')
        || value.ends_with('-')
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
    {
        return Err(WinWinCodeExportError::InvalidContent);
    }
    Ok(())
}

fn content_digest(export_id: &str, content: &WinWinCodeExportContent) -> String {
    let bytes = encode_digest_material(export_id, content);
    encode_sha256(&Sha256::digest(bytes))
}

fn encode_bounded(export: &WinWinCodeExport) -> Result<Vec<u8>, WinWinCodeExportError> {
    let bytes = encode_document(export);
    if bytes.len() > MAX_WINWINCODE_EXPORT_BYTES {
        return Err(WinWinCodeExportError::TooLarge);
    }
    Ok(bytes)
}

fn encode_document(export: &WinWinCodeExport) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"{\"format\":");
    encode_json_string(&mut bytes, &export.format);
    bytes.extend_from_slice(b",\"exportId\":");
    encode_json_string(&mut bytes, &export.export_id);
    bytes.extend_from_slice(b",\"contentSha256\":");
    encode_json_string(&mut bytes, &export.content_sha256);
    bytes.extend_from_slice(b",\"content\":");
    encode_content(&mut bytes, &export.content);
    bytes.push(b'}');
    bytes
}

fn encode_digest_material(export_id: &str, content: &WinWinCodeExportContent) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"{\"format\":");
    encode_json_string(&mut bytes, WINWINCODE_EXPORT_FORMAT);
    bytes.extend_from_slice(b",\"exportId\":");
    encode_json_string(&mut bytes, export_id);
    bytes.extend_from_slice(b",\"content\":");
    encode_content(&mut bytes, content);
    bytes.push(b'}');
    bytes
}

fn encode_content(bytes: &mut Vec<u8>, content: &WinWinCodeExportContent) {
    bytes.extend_from_slice(b"{\"profileDisplayName\":");
    encode_json_string(bytes, &content.profile_display_name);
    bytes.extend_from_slice(b",\"organizations\":[");
    for (index, organization) in content.organizations.iter().enumerate() {
        if index > 0 {
            bytes.push(b',');
        }
        encode_organization(bytes, organization);
    }
    bytes.extend_from_slice(b"],\"projects\":[");
    for (index, project) in content.projects.iter().enumerate() {
        if index > 0 {
            bytes.push(b',');
        }
        encode_project(bytes, project);
    }
    bytes.extend_from_slice(b"]}");
}

fn encode_organization(bytes: &mut Vec<u8>, organization: &WinWinCodeExportOrganization) {
    bytes.extend_from_slice(b"{\"sourceOrganizationId\":");
    encode_json_string(bytes, &organization.source_organization_id);
    bytes.extend_from_slice(b",\"slug\":");
    encode_json_string(bytes, &organization.slug);
    bytes.extend_from_slice(b",\"displayName\":");
    encode_json_string(bytes, &organization.display_name);
    bytes.push(b'}');
}

fn encode_project(bytes: &mut Vec<u8>, project: &WinWinCodeExportProject) {
    bytes.extend_from_slice(b"{\"sourceProjectId\":");
    encode_json_string(bytes, &project.source_project_id);
    bytes.extend_from_slice(b",\"sourceOrganizationId\":");
    encode_json_string(bytes, &project.source_organization_id);
    bytes.extend_from_slice(b",\"slug\":");
    encode_json_string(bytes, &project.slug);
    bytes.extend_from_slice(b",\"displayName\":");
    encode_json_string(bytes, &project.display_name);
    bytes.push(b'}');
}

fn encode_json_string(bytes: &mut Vec<u8>, value: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    bytes.push(b'"');
    for character in value.chars() {
        match character {
            '"' => bytes.extend_from_slice(br#"\""#),
            '\\' => bytes.extend_from_slice(br"\\"),
            '\u{0008}' => bytes.extend_from_slice(br"\b"),
            '\t' => bytes.extend_from_slice(br"\t"),
            '\n' => bytes.extend_from_slice(br"\n"),
            '\u{000c}' => bytes.extend_from_slice(br"\f"),
            '\r' => bytes.extend_from_slice(br"\r"),
            '\u{0000}'..='\u{001f}' => {
                let code = character as usize;
                bytes.extend_from_slice(br"\u00");
                bytes.push(HEX[code >> 4]);
                bytes.push(HEX[code & 0x0f]);
            }
            _ => {
                let mut encoded = [0; 4];
                bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            }
        }
    }
    bytes.push(b'"');
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn encode_sha256(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::{MAX_WINWINCODE_EXPORT_BYTES, WinWinCodeExportError, encode_json_string};

    const CANONICAL_STRING_FIXTURE: &[u8] = include_bytes!(
        "../../../schema/winwincode-export/v1/canonical-json-string.example.json.bytes"
    );

    fn encode_string_probe(value: &str) -> Result<Vec<u8>, WinWinCodeExportError> {
        let mut bytes = br#"{"value":"#.to_vec();
        encode_json_string(&mut bytes, value);
        bytes.push(b'}');
        if bytes.len() > MAX_WINWINCODE_EXPORT_BYTES {
            return Err(WinWinCodeExportError::TooLarge);
        }
        Ok(bytes)
    }

    fn replace_bytes_once(source: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
        let start = source
            .windows(from.len())
            .position(|window| window == from)
            .expect("fixture contains replacement source");
        let mut replaced = Vec::with_capacity(source.len() - from.len() + to.len());
        replaced.extend_from_slice(&source[..start]);
        replaced.extend_from_slice(to);
        replaced.extend_from_slice(&source[start + from.len()..]);
        replaced
    }

    #[test]
    fn compact_utf8_byte_limit_accepts_the_boundary_and_rejects_the_next_byte() {
        let empty_size = br#"{"value":""}"#.len();
        let at_limit = "x".repeat(MAX_WINWINCODE_EXPORT_BYTES - empty_size);
        assert_eq!(
            encode_string_probe(&at_limit)
                .expect("exact byte boundary")
                .len(),
            MAX_WINWINCODE_EXPORT_BYTES
        );

        let over_limit = "x".repeat(MAX_WINWINCODE_EXPORT_BYTES - empty_size + 1);
        assert_eq!(
            encode_string_probe(&over_limit),
            Err(WinWinCodeExportError::TooLarge)
        );
    }

    #[test]
    fn canonical_json_string_fixture_covers_every_escape_class() {
        let value: String =
            serde_json::from_slice(CANONICAL_STRING_FIXTURE).expect("canonical string fixture");
        let mut encoded = Vec::new();
        encode_json_string(&mut encoded, &value);
        assert_eq!(encoded, CANONICAL_STRING_FIXTURE);

        for noncanonical in [
            replace_bytes_once(CANONICAL_STRING_FIXTURE, br"\b", br"\u0008"),
            replace_bytes_once(CANONICAL_STRING_FIXTURE, br"\n", br"\u000a"),
            replace_bytes_once(CANONICAL_STRING_FIXTURE, "é".as_bytes(), br"\u00e9"),
            replace_bytes_once(CANONICAL_STRING_FIXTURE, "🦀".as_bytes(), br"\ud83e\udd80"),
        ] {
            let same_value: String =
                serde_json::from_slice(&noncanonical).expect("equivalent JSON string");
            assert_eq!(same_value, value);
            let mut normalized = Vec::new();
            encode_json_string(&mut normalized, &same_value);
            assert_eq!(normalized, CANONICAL_STRING_FIXTURE);
            assert_ne!(noncanonical, CANONICAL_STRING_FIXTURE);
        }
    }
}
