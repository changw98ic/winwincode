// SPDX-License-Identifier: Apache-2.0

//! Signed identity for the bundled Kernel helper.
//!
//! The helper executable is an execution boundary.  Its identity therefore
//! comes from a release manifest signed by the release key and from the
//! public key compiled into this crate.  Callers only select the manifest and
//! executable paths; they cannot supply a replacement digest.

use std::fmt;
use std::fs::OpenOptions;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use serde::Deserialize;
use winwincode_domain::Sha256Digest;

#[cfg(any(test, feature = "test-support"))]
use sha2::{Digest as _, Sha256};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
#[cfg(all(unix, any(test, feature = "test-support")))]
use std::os::unix::fs::PermissionsExt as _;

pub(crate) const HELPER_RELEASE_MANIFEST_NAME: &str = "winwincode-kernel-helper.release.json";
pub(crate) const HELPER_RELEASE_BINARY_NAME: &str = "winwincode-kernel-helper";
pub(crate) const HELPER_RELEASE_BINARY_MODE: u32 = 0o755;
/// Maximum helper image accepted by the Production Codex boundary.
pub(crate) const MAX_HELPER_BYTES: u64 = 64 * 1024 * 1024;
const MANIFEST_SCHEMA_VERSION: u32 = 1;
const MANIFEST_PROTOCOL: &str = "winwincode-kernel-helper-release";
const MANIFEST_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 16 * 1024;
const SIGNING_PREFIX: &str = "winwincode-kernel-helper.release.v1";

const COMPILED_PUBLIC_KEY_HEX: &str = env!("WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX");
const COMPILED_HELPER_SOURCE_SHA256: &str = env!("WINWINCODE_HELPER_SOURCE_SHA256");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestDocument {
    schema_version: u32,
    protocol: String,
    version: u32,
    package_version: String,
    source_sha256: String,
    binary_sha256: String,
    binary_path: String,
    binary_mode: u32,
    signature: String,
}

struct SigningFields<'a> {
    schema_version: u32,
    protocol: &'a str,
    version: u32,
    package_version: &'a str,
    source_sha256: &'a str,
    binary_sha256: &'a str,
    binary_path: &'a str,
    binary_mode: u32,
}

/// Authenticated release identity for one exact helper binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperReleaseManifest {
    canonical_path: PathBuf,
    package_version: String,
    source_sha256: String,
    binary_digest: Sha256Digest,
    binary_path: String,
    binary_mode: u32,
    signature: [u8; 64],
}

impl HelperReleaseManifest {
    /// Load and authenticate a release manifest from disk.
    ///
    /// The manifest itself must be a regular file with the canonical release
    /// name.  Its signature, helper source identity, package version, and
    /// binary digest are checked before a value is returned.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest is missing, malformed, or fails any
    /// identity, digest, permission, or signature check.
    pub fn from_file(path: &Path) -> Result<Self, HelperReleaseManifestError> {
        let canonical_path = canonical_manifest_path(path)?;
        let document = parse_manifest(&canonical_path)?;
        let signature = decode_signature(&document.signature)?;
        let binary_digest = validate_digest(&document.binary_sha256)?;
        if document.schema_version != MANIFEST_SCHEMA_VERSION
            || document.protocol != MANIFEST_PROTOCOL
            || document.version != MANIFEST_VERSION
            || document.package_version != env!("CARGO_PKG_VERSION")
            || document.source_sha256 != COMPILED_HELPER_SOURCE_SHA256
            || document.binary_path != HELPER_RELEASE_BINARY_NAME
            || document.binary_mode != HELPER_RELEASE_BINARY_MODE
        {
            return Err(HelperReleaseManifestError::invalid());
        }
        let signing_bytes = signing_bytes(&SigningFields {
            schema_version: document.schema_version,
            protocol: &document.protocol,
            version: document.version,
            package_version: &document.package_version,
            source_sha256: &document.source_sha256,
            binary_sha256: &document.binary_sha256,
            binary_path: &document.binary_path,
            binary_mode: document.binary_mode,
        });
        verify_signature(&signature, &signing_bytes)?;
        Ok(Self {
            canonical_path,
            package_version: document.package_version,
            source_sha256: document.source_sha256,
            binary_digest,
            binary_path: document.binary_path,
            binary_mode: document.binary_mode,
            signature,
        })
    }

    /// Absolute canonical path of the authenticated manifest.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.canonical_path
    }

    /// Package version bound into the signed helper identity.
    #[must_use]
    pub fn package_version(&self) -> &str {
        &self.package_version
    }

    /// Source digest bound into the signed helper identity.
    #[must_use]
    pub fn source_sha256(&self) -> &str {
        &self.source_sha256
    }

    /// Relative path of the helper binary inside the release directory.
    #[must_use]
    pub fn binary_path(&self) -> &str {
        &self.binary_path
    }

    /// Exact source mode bound by the release signature.
    #[must_use]
    pub const fn binary_mode(&self) -> u32 {
        self.binary_mode
    }

    pub(crate) fn binary_digest(&self) -> &Sha256Digest {
        &self.binary_digest
    }

    /// Build a signed manifest value for the checked-in test helper.
    ///
    /// This constructor is only compiled for test-support consumers.  The
    /// production path always loads the signed file emitted by the release
    /// build and never accepts a caller-computed digest.
    ///
    /// # Errors
    ///
    /// Returns an error when the helper path is not the exact checked-in test
    /// helper or when reading it fails validation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_test_helper(path: &Path) -> Result<Self, HelperReleaseManifestError> {
        let canonical_helper = path
            .canonicalize()
            .map_err(|_| HelperReleaseManifestError::invalid())?;
        if canonical_helper.file_name().and_then(|name| name.to_str())
            != Some(HELPER_RELEASE_BINARY_NAME)
        {
            return Err(HelperReleaseManifestError::invalid());
        }
        #[cfg(unix)]
        if !std::fs::symlink_metadata(&canonical_helper).is_ok_and(|metadata| {
            metadata.permissions().mode() & 0o777 == HELPER_RELEASE_BINARY_MODE
        }) {
            return Err(HelperReleaseManifestError::invalid());
        }
        let bytes = read_regular_file(&canonical_helper)?;
        let binary_sha256 = format!("sha256:{:x}", Sha256::digest(&bytes));
        let package_version = env!("CARGO_PKG_VERSION").to_owned();
        let source_sha256 = COMPILED_HELPER_SOURCE_SHA256.to_owned();
        let signing = signing_bytes(&SigningFields {
            schema_version: MANIFEST_SCHEMA_VERSION,
            protocol: MANIFEST_PROTOCOL,
            version: MANIFEST_VERSION,
            package_version: &package_version,
            source_sha256: &source_sha256,
            binary_sha256: &binary_sha256,
            binary_path: HELPER_RELEASE_BINARY_NAME,
            binary_mode: HELPER_RELEASE_BINARY_MODE,
        });
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[42_u8; 32]);
        let signature = ed25519_dalek::Signer::sign(&signing_key, &signing).to_bytes();
        let canonical_path = canonical_helper
            .parent()
            .ok_or_else(HelperReleaseManifestError::invalid)?
            .join(HELPER_RELEASE_MANIFEST_NAME);
        Ok(Self {
            canonical_path,
            package_version,
            source_sha256,
            binary_digest: Sha256Digest(binary_sha256),
            binary_path: HELPER_RELEASE_BINARY_NAME.to_owned(),
            binary_mode: HELPER_RELEASE_BINARY_MODE,
            signature,
        })
    }
}

/// Secret-safe manifest parse/authentication failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelperReleaseManifestError;

impl HelperReleaseManifestError {
    const fn invalid() -> Self {
        Self
    }
}

impl fmt::Display for HelperReleaseManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the Kernel helper release manifest is invalid")
    }
}

impl std::error::Error for HelperReleaseManifestError {}

fn canonical_manifest_path(path: &Path) -> Result<PathBuf, HelperReleaseManifestError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| HelperReleaseManifestError::invalid())?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || path.file_name().and_then(|name| name.to_str()) != Some(HELPER_RELEASE_MANIFEST_NAME)
    {
        return Err(HelperReleaseManifestError::invalid());
    }
    path.canonicalize()
        .map_err(|_| HelperReleaseManifestError::invalid())
}

fn parse_manifest(path: &Path) -> Result<ManifestDocument, HelperReleaseManifestError> {
    let mut file = OpenOptions::new();
    file.read(true);
    #[cfg(unix)]
    file.custom_flags(if cfg!(target_os = "macos") {
        // Darwin O_NOFOLLOW.
        0x100
    } else {
        // Linux O_NOFOLLOW.  Release targets are Darwin and Linux.
        0x20_000
    });
    let file = file
        .open(path)
        .map_err(|_| HelperReleaseManifestError::invalid())?;
    let metadata = file
        .metadata()
        .map_err(|_| HelperReleaseManifestError::invalid())?;
    if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(HelperReleaseManifestError::invalid());
    }
    let capacity =
        usize::try_from(metadata.len()).map_err(|_| HelperReleaseManifestError::invalid())?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take((MAX_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| HelperReleaseManifestError::invalid())?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(HelperReleaseManifestError::invalid());
    }
    serde_json::from_slice(&bytes).map_err(|_| HelperReleaseManifestError::invalid())
}

fn validate_digest(value: &str) -> Result<Sha256Digest, HelperReleaseManifestError> {
    if value.len() != "sha256:".len() + 64
        || !value.starts_with("sha256:")
        || !value["sha256:".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(HelperReleaseManifestError::invalid());
    }
    Ok(Sha256Digest(value.to_owned()))
}

fn decode_signature(value: &str) -> Result<[u8; 64], HelperReleaseManifestError> {
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| HelperReleaseManifestError::invalid())?;
    decoded
        .try_into()
        .map_err(|_| HelperReleaseManifestError::invalid())
}

fn verify_signature(
    signature: &[u8; 64],
    signing_bytes: &[u8],
) -> Result<(), HelperReleaseManifestError> {
    let public_key_bytes = decode_hex_32(COMPILED_PUBLIC_KEY_HEX)?;
    let public_key = VerifyingKey::from_bytes(&public_key_bytes)
        .map_err(|_| HelperReleaseManifestError::invalid())?;
    let signature = Signature::from_bytes(signature);
    public_key
        .verify(signing_bytes, &signature)
        .map_err(|_| HelperReleaseManifestError::invalid())
}

fn decode_hex_32(value: &str) -> Result<[u8; 32], HelperReleaseManifestError> {
    if value.len() != 64 {
        return Err(HelperReleaseManifestError::invalid());
    }
    let mut bytes = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(bytes)
}

fn hex_nibble(value: u8) -> Result<u8, HelperReleaseManifestError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(HelperReleaseManifestError::invalid()),
    }
}

fn signing_bytes(fields: &SigningFields<'_>) -> Vec<u8> {
    [
        SIGNING_PREFIX,
        &fields.schema_version.to_string(),
        fields.protocol,
        &fields.version.to_string(),
        fields.package_version,
        fields.source_sha256,
        fields.binary_sha256,
        fields.binary_path,
        &fields.binary_mode.to_string(),
    ]
    .join("\0")
    .into_bytes()
}

#[cfg(any(test, feature = "test-support"))]
fn read_regular_file(path: &Path) -> Result<Vec<u8>, HelperReleaseManifestError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| HelperReleaseManifestError::invalid())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(HelperReleaseManifestError::invalid());
    }
    let mut file = OpenOptions::new();
    file.read(true);
    #[cfg(unix)]
    file.custom_flags(if cfg!(target_os = "macos") {
        0x100
    } else {
        0x20_000
    });
    let file = file
        .open(path)
        .map_err(|_| HelperReleaseManifestError::invalid())?;
    let opened_metadata = file
        .metadata()
        .map_err(|_| HelperReleaseManifestError::invalid())?;
    if !opened_metadata.is_file() || opened_metadata.len() > MAX_HELPER_BYTES {
        return Err(HelperReleaseManifestError::invalid());
    }
    let capacity = usize::try_from(opened_metadata.len())
        .map_err(|_| HelperReleaseManifestError::invalid())?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_HELPER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| HelperReleaseManifestError::invalid())?;
    if bytes.len() as u64 > MAX_HELPER_BYTES {
        return Err(HelperReleaseManifestError::invalid());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        HELPER_RELEASE_BINARY_MODE, HELPER_RELEASE_BINARY_NAME, HELPER_RELEASE_MANIFEST_NAME,
        HelperReleaseManifest, MANIFEST_PROTOCOL, MANIFEST_SCHEMA_VERSION, MANIFEST_VERSION,
    };
    use base64::Engine as _;
    use serde_json::json;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn signed_manifest_round_trips_and_tampering_fails_closed() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-helper-release-manifest-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create helper manifest fixture");
        let helper = root.join("winwincode-kernel-helper");
        std::fs::write(&helper, b"test helper bytes").expect("write helper fixture");
        #[cfg(unix)]
        std::fs::set_permissions(
            &helper,
            std::fs::Permissions::from_mode(HELPER_RELEASE_BINARY_MODE),
        )
        .expect("make helper fixture executable");
        let signed =
            HelperReleaseManifest::from_test_helper(&helper).expect("create signed test manifest");
        let binary_sha256 = signed.binary_digest().0.clone();
        let document = json!({
            "schemaVersion": MANIFEST_SCHEMA_VERSION,
            "protocol": MANIFEST_PROTOCOL,
            "version": MANIFEST_VERSION,
            "packageVersion": signed.package_version(),
            "sourceSha256": signed.source_sha256(),
            "binarySha256": binary_sha256,
            "binaryPath": HELPER_RELEASE_BINARY_NAME,
            "binaryMode": HELPER_RELEASE_BINARY_MODE,
            "signature": base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(signed.signature),
        });
        let manifest_path = root.join(HELPER_RELEASE_MANIFEST_NAME);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&document).expect("encode manifest fixture"),
        )
        .expect("write manifest fixture");
        let loaded = HelperReleaseManifest::from_file(&manifest_path)
            .expect("verify signed manifest fixture");
        assert_eq!(loaded, signed);

        let mut tampered = document;
        tampered["binarySha256"] = json!(format!("sha256:{}", "0".repeat(64)));
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&tampered).expect("encode tampered manifest"),
        )
        .expect("write tampered manifest");
        assert!(HelperReleaseManifest::from_file(&manifest_path).is_err());
        std::fs::remove_dir_all(root).expect("remove helper manifest fixture");
    }
}
