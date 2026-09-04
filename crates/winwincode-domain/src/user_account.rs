// SPDX-License-Identifier: Apache-2.0

//! The `UserAccount` domain object for the multi-user login surface.
//!
//! One `UserAccount` is the durable identity of one human operator. The
//! fields mirror the multi-user plan section 7.1: the user-chosen
//! `username`, the uniqueness-bearing `normalizedUsername`, the opaque
//! Argon2id PHC `passwordHash` string, the `owner | member` role, the
//! `active | disabled` lifecycle state, and the optimistic-concurrency
//! `revision`.
//!
//! This model owns only the stored invariants. Username normalization and
//! password hashing stay in their own layers; the uniqueness of
//! `normalizedUsername` is enforced by the storage adapter, not here.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Instant, Revision, UserId};

/// Maximum byte length accepted for `username` and `normalizedUsername`.
const MAX_USERNAME_BYTES: usize = 96;

/// Maximum byte length accepted for an Argon2id PHC `passwordHash` string.
const MAX_PASSWORD_HASH_BYTES: usize = 512;

/// Administration role of one `UserAccount`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum UserAccountRole {
    /// The first accountable operator; may administer other accounts.
    #[serde(rename = "owner")]
    Owner,
    /// A regular operator created by an Owner.
    #[serde(rename = "member")]
    Member,
}

impl UserAccountRole {
    /// Canonical stored spelling of this role.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Member => "member",
        }
    }

    /// Parses one canonical stored role spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "member" => Some(Self::Member),
            _ => None,
        }
    }
}

/// Lifecycle state of one `UserAccount`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum UserAccountState {
    /// Login and session use are allowed.
    #[serde(rename = "active")]
    Active,
    /// Login is refused and every session must be revoked.
    #[serde(rename = "disabled")]
    Disabled,
}

impl UserAccountState {
    /// Canonical stored spelling of this state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }

    /// Parses one canonical stored state spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// Validation failure category for one `UserAccount` value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserAccountErrorKind {
    /// `userId` is not a canonical `usr_` identifier.
    InvalidUserId,
    /// `username` is empty, overlong, whitespace-padded, or control-bearing.
    InvalidUsername,
    /// `normalizedUsername` is not a normalized uniqueness key.
    InvalidNormalizedUsername,
    /// `passwordHash` is not an Argon2id PHC string.
    InvalidPasswordHash,
    /// A timestamp is not canonical, or `updatedAt` precedes `createdAt`.
    InvalidTimestamp,
    /// `revision` is not positive.
    InvalidRevision,
}

/// Validation failure for one `UserAccount` value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserAccountError {
    kind: UserAccountErrorKind,
    message: String,
}

impl UserAccountError {
    /// Closed failure category of this validation failure.
    #[must_use]
    pub const fn kind(&self) -> UserAccountErrorKind {
        self.kind
    }
}

impl fmt::Display for UserAccountError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UserAccountError {}

/// One durable human login account.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserAccount {
    /// Stable account identity.
    #[serde(rename = "userId")]
    pub user_id: UserId,
    /// Chosen login name exactly as entered by the operator.
    #[serde(rename = "username")]
    pub username: String,
    /// Normalized uniqueness key for the username; unique across accounts.
    #[serde(rename = "normalizedUsername")]
    pub normalized_username: String,
    /// Argon2id PHC string; never a plaintext password.
    #[serde(rename = "passwordHash")]
    pub password_hash: String,
    /// Administration role.
    #[serde(rename = "role")]
    pub role: UserAccountRole,
    /// Lifecycle state.
    #[serde(rename = "state")]
    pub state: UserAccountState,
    /// Canonical creation instant.
    #[serde(rename = "createdAt")]
    pub created_at: Instant,
    /// Canonical last-update instant; never earlier than `createdAt`.
    #[serde(rename = "updatedAt")]
    pub updated_at: Instant,
    /// Optimistic-concurrency revision; increases with every durable update.
    #[serde(rename = "revision")]
    pub revision: Revision,
}

impl UserAccount {
    /// Builds one validated `UserAccount`.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical `usr_` identifier, an empty, overlong,
    /// whitespace-padded, or control-bearing username, a `normalizedUsername`
    /// that still contains whitespace or uppercase characters, a
    /// `passwordHash` that is not an Argon2id PHC string, a non-canonical
    /// timestamp, an `updatedAt` earlier than `createdAt`, or a revision
    /// that is not positive.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        user_id: UserId,
        username: String,
        normalized_username: String,
        password_hash: String,
        role: UserAccountRole,
        state: UserAccountState,
        created_at: Instant,
        updated_at: Instant,
        revision: Revision,
    ) -> Result<Self, UserAccountError> {
        if !canonical_user_id(&user_id.0) {
            return Err(error(
                UserAccountErrorKind::InvalidUserId,
                "userId is not a canonical usr_ identifier",
            ));
        }
        if invalid_username(&username) {
            return Err(error(
                UserAccountErrorKind::InvalidUsername,
                "username must be non-empty, unpadded, and at most 96 bytes",
            ));
        }
        if invalid_normalized_username(&normalized_username) {
            return Err(error(
                UserAccountErrorKind::InvalidNormalizedUsername,
                "normalizedUsername must be non-empty, lowercase, and whitespace-free",
            ));
        }
        if !canonical_password_hash(&password_hash) {
            return Err(error(
                UserAccountErrorKind::InvalidPasswordHash,
                "passwordHash must be an Argon2id PHC string",
            ));
        }
        if !canonical_instant(&created_at.0) || !canonical_instant(&updated_at.0) {
            return Err(error(
                UserAccountErrorKind::InvalidTimestamp,
                "timestamps must be canonical yyyy-MM-ddTHH:mm:ss.mmmZ instants",
            ));
        }
        if updated_at.0.as_bytes() < created_at.0.as_bytes() {
            return Err(error(
                UserAccountErrorKind::InvalidTimestamp,
                "updatedAt must not be earlier than createdAt",
            ));
        }
        if revision.0 < 1 {
            return Err(error(
                UserAccountErrorKind::InvalidRevision,
                "revision must be positive",
            ));
        }
        Ok(Self {
            user_id,
            username,
            normalized_username,
            password_hash,
            role,
            state,
            created_at,
            updated_at,
            revision,
        })
    }
}

fn canonical_user_id(value: &str) -> bool {
    value.strip_prefix("usr_").is_some_and(|identifier| {
        identifier.len() == 26
            && identifier.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
            })
    })
}

fn invalid_username(value: &str) -> bool {
    value.is_empty()
        || value.len() > MAX_USERNAME_BYTES
        || value.trim() != value
        || value.bytes().any(|byte| byte.is_ascii_control())
}

fn invalid_normalized_username(value: &str) -> bool {
    value.is_empty()
        || value.len() > MAX_USERNAME_BYTES
        || value.bytes().any(|byte| byte.is_ascii_whitespace())
        || value.to_lowercase() != *value
}

fn canonical_password_hash(value: &str) -> bool {
    if value.len() > MAX_PASSWORD_HASH_BYTES {
        return false;
    }
    let Some(rest) = value.strip_prefix("$argon2id$") else {
        return false;
    };
    let segments: Vec<&str> = rest.split('$').collect();
    segments.len() == 4
        && segments[0].strip_prefix("v=").is_some_and(|version| {
            !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit())
        })
        && segments[1..].iter().all(|segment| {
            !segment.is_empty() && !segment.bytes().any(|byte| byte.is_ascii_whitespace())
        })
}

fn canonical_instant(value: &str) -> bool {
    let bytes = value.as_bytes();
    let punctuation = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'.'),
        (23, b'Z'),
    ];
    bytes.len() == 24
        && punctuation
            .iter()
            .all(|(index, byte)| bytes[*index] == *byte)
        && bytes.iter().enumerate().all(|(index, byte)| {
            punctuation.iter().any(|(at, _)| at == &index) || byte.is_ascii_digit()
        })
}

fn error(kind: UserAccountErrorKind, message: impl Into<String>) -> UserAccountError {
    UserAccountError {
        kind,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHC_HASH: &str =
        "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$RdescudvJCsgt3ub+b+dWRWJTmaaJObG";
    const USER_ID: &str = "usr_00000000000000000000000001";
    const CREATED_AT: &str = "2027-05-01T08:00:00.000Z";
    const UPDATED_AT: &str = "2027-05-01T08:00:00.001Z";

    fn build(
        user_id: &str,
        username: &str,
        normalized_username: &str,
        password_hash: &str,
        created_at: &str,
        updated_at: &str,
        revision: i64,
    ) -> Result<UserAccount, UserAccountError> {
        UserAccount::new(
            UserId(user_id.to_owned()),
            username.to_owned(),
            normalized_username.to_owned(),
            password_hash.to_owned(),
            UserAccountRole::Owner,
            UserAccountState::Active,
            Instant(created_at.to_owned()),
            Instant(updated_at.to_owned()),
            Revision(revision),
        )
    }

    #[test]
    fn builds_a_valid_account() {
        let account = build(USER_ID, "Wen", "wen", PHC_HASH, CREATED_AT, UPDATED_AT, 1)
            .expect("valid user account");
        assert_eq!(account.role.as_str(), "owner");
        assert_eq!(account.state.as_str(), "active");
        assert_eq!(account.user_id.0, USER_ID);
    }

    #[test]
    fn rejects_invalid_user_ids() {
        let built = build(
            "wrk_00000000000000000000000001",
            "Wen",
            "wen",
            PHC_HASH,
            CREATED_AT,
            UPDATED_AT,
            1,
        )
        .expect_err("invalid user account");
        assert_eq!(built.kind(), UserAccountErrorKind::InvalidUserId);
    }

    #[test]
    fn rejects_invalid_usernames() {
        let overlong = "a".repeat(MAX_USERNAME_BYTES + 1);
        for username in ["", " padded", "padded ", overlong.as_str()] {
            let built = build(
                USER_ID, username, "wen", PHC_HASH, CREATED_AT, UPDATED_AT, 1,
            )
            .expect_err("invalid user account");
            assert_eq!(built.kind(), UserAccountErrorKind::InvalidUsername);
        }
    }

    #[test]
    fn rejects_unnormalized_usernames() {
        for normalized_username in ["Wen", "we n", " wen"] {
            let built = build(
                USER_ID,
                "Wen",
                normalized_username,
                PHC_HASH,
                CREATED_AT,
                UPDATED_AT,
                1,
            )
            .expect_err("invalid user account");
            assert_eq!(
                built.kind(),
                UserAccountErrorKind::InvalidNormalizedUsername
            );
        }
    }

    #[test]
    fn rejects_non_argon2id_password_hashes() {
        for password_hash in [
            "plain-text-password",
            "$argon2i$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$RdescudvJCsgt3ub",
            "$argon2id$m=19456,t=2,p=1$c29tZXNhbHQ$RdescudvJCsgt3ub",
            "$argon2id$v=19$m=19456,t=2,p=1$",
        ] {
            let built = build(
                USER_ID,
                "Wen",
                "wen",
                password_hash,
                CREATED_AT,
                UPDATED_AT,
                1,
            )
            .expect_err("invalid user account");
            assert_eq!(built.kind(), UserAccountErrorKind::InvalidPasswordHash);
        }
    }

    #[test]
    fn rejects_non_canonical_or_unordered_timestamps() {
        for (created_at, updated_at) in [
            ("2027-05-01 08:00:00.000", UPDATED_AT),
            ("2027-05-01T08:00:00Z", UPDATED_AT),
            (UPDATED_AT, CREATED_AT),
        ] {
            let built = build(USER_ID, "Wen", "wen", PHC_HASH, created_at, updated_at, 1)
                .expect_err("invalid user account");
            assert_eq!(built.kind(), UserAccountErrorKind::InvalidTimestamp);
        }
    }

    #[test]
    fn rejects_non_positive_revisions() {
        for revision in [0, -1] {
            let built = build(
                USER_ID, "Wen", "wen", PHC_HASH, CREATED_AT, UPDATED_AT, revision,
            )
            .expect_err("invalid user account");
            assert_eq!(built.kind(), UserAccountErrorKind::InvalidRevision);
        }
    }

    #[test]
    fn role_and_state_enums_round_trip_their_canonical_spellings() {
        for (role, spelling) in [
            (UserAccountRole::Owner, "owner"),
            (UserAccountRole::Member, "member"),
        ] {
            assert_eq!(role.as_str(), spelling);
            assert_eq!(UserAccountRole::parse(spelling), Some(role));
        }
        for (state, spelling) in [
            (UserAccountState::Active, "active"),
            (UserAccountState::Disabled, "disabled"),
        ] {
            assert_eq!(state.as_str(), spelling);
            assert_eq!(UserAccountState::parse(spelling), Some(state));
        }
        assert_eq!(UserAccountRole::parse("administrator"), None);
        assert_eq!(UserAccountState::parse("archived"), None);
    }

    #[test]
    fn reports_error_kinds_through_display_and_kind() {
        let built = build(
            USER_ID,
            "Wen",
            "wen",
            "not-a-phc",
            CREATED_AT,
            UPDATED_AT,
            1,
        )
        .expect_err("invalid user account");
        assert_eq!(built.kind(), UserAccountErrorKind::InvalidPasswordHash);
        assert_eq!(
            built.to_string(),
            "passwordHash must be an Argon2id PHC string"
        );
    }
}
