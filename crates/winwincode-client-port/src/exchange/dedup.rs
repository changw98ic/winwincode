// SPDX-License-Identifier: Apache-2.0

//! Frame deduplication and command idempotency-conflict judgement.
//!
//! The contract fixes two distinct identities and their outcomes:
//!
//! - frame replay: the same `messageId`, `sequence`, and payload digest
//!   re-arrives → `duplicate` (confirm the original frame, never re-execute);
//! - idempotency conflict: the same `idempotencyKey` arrives with a different
//!   payload digest → `rejected_conflict` (never overwrite accepted data).
//!
//! [`DedupRegister`] is the in-memory judge for both rules. Durable adapters
//! (device-client persistence lane) persist the same two records and apply
//! the same judgement; the rules themselves live only here.

use std::collections::HashMap;

use crate::exchange::frame::FrameIdentity;

/// The command idempotency identity every command carries on top of the frame
/// identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandIdentity {
    /// Sender-generated idempotency key (`idempotencyKey`).
    pub idempotency_key: String,
    /// Digest of the canonical payload bytes the key was generated for.
    pub payload_digest: String,
}

impl CommandIdentity {
    /// Builds one command identity.
    #[must_use]
    pub fn new(idempotency_key: impl Into<String>, payload_digest: impl Into<String>) -> Self {
        Self {
            idempotency_key: idempotency_key.into(),
            payload_digest: payload_digest.into(),
        }
    }
}

/// Verdict for one frame or command identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupVerdict {
    /// The identity has not been recorded; the caller may process it.
    New,
    /// The exact same identity and payload digest were seen; confirm the
    /// original outcome without executing again.
    Duplicate,
    /// The identity was reused with a different payload digest; never
    /// overwrite the accepted outcome.
    Conflict,
}

/// In-memory record of accepted frame and command identities.
///
/// Only accepted frames and commands are recorded. Frame identities may be
/// compacted once the acknowledgement covers them; command identities must be
/// kept for as long as their idempotency guarantee must hold.
#[derive(Debug, Default, Clone)]
pub struct DedupRegister {
    frames: HashMap<FrameKey, String>,
    commands: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FrameKey {
    message_id: String,
    sequence: u64,
}

impl DedupRegister {
    /// Creates an empty register.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Judges one frame identity against the accepted records.
    ///
    /// A frame identity is the `(messageId, sequence)` pair; the verdict is
    /// `Duplicate` only when the retained payload digest also matches.
    #[must_use]
    pub fn check_frame(&self, identity: &FrameIdentity) -> DedupVerdict {
        let key = FrameKey {
            message_id: identity.message_id.clone(),
            sequence: identity.sequence,
        };
        match self.frames.get(&key) {
            None => DedupVerdict::New,
            Some(digest) if *digest == identity.payload_digest => DedupVerdict::Duplicate,
            Some(_) => DedupVerdict::Conflict,
        }
    }

    /// Records one accepted frame identity.
    pub fn record_frame(&mut self, identity: &FrameIdentity) {
        let key = FrameKey {
            message_id: identity.message_id.clone(),
            sequence: identity.sequence,
        };
        self.frames.insert(key, identity.payload_digest.clone());
    }

    /// Judges one command identity against the accepted records.
    ///
    /// The same key with the same payload digest is an idempotent replay
    /// (`Duplicate`); the same key with a different payload digest is a
    /// `rejected_conflict` (`Conflict`).
    #[must_use]
    pub fn check_command(&self, command: &CommandIdentity) -> DedupVerdict {
        match self.commands.get(&command.idempotency_key) {
            None => DedupVerdict::New,
            Some(digest) if *digest == command.payload_digest => DedupVerdict::Duplicate,
            Some(_) => DedupVerdict::Conflict,
        }
    }

    /// Records one accepted command identity.
    pub fn record_command(&mut self, command: &CommandIdentity) {
        self.commands.insert(
            command.idempotency_key.clone(),
            command.payload_digest.clone(),
        );
    }

    /// Forgets frame identities at or below `sequence`.
    ///
    /// Callers may compact records covered by a persisted acknowledgement:
    /// frames at or below the contiguous acknowledgement are duplicates by
    /// sequence position, so the retained record is only an integrity check.
    /// Command records are never compacted here.
    pub fn compact_frames_through(&mut self, sequence: u64) -> usize {
        let before = self.frames.len();
        self.frames.retain(|key, _| key.sequence > sequence);
        before - self.frames.len()
    }

    /// Returns how many frame identities are retained.
    #[must_use]
    pub fn retained_frames(&self) -> usize {
        self.frames.len()
    }

    /// Returns how many command identities are retained.
    #[must_use]
    pub fn retained_commands(&self) -> usize {
        self.commands.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(message_id: &str, sequence: u64, digest: &str) -> FrameIdentity {
        FrameIdentity::new(message_id, sequence, digest)
    }

    #[test]
    fn frame_identity_starts_new_then_duplicates() {
        let mut register = DedupRegister::new();
        let identity = frame("msg-1", 1, "sha256:aa");
        assert_eq!(register.check_frame(&identity), DedupVerdict::New);
        register.record_frame(&identity);
        assert_eq!(register.check_frame(&identity), DedupVerdict::Duplicate);
    }

    #[test]
    fn frame_identity_reuse_with_a_different_digest_conflicts() {
        let mut register = DedupRegister::new();
        register.record_frame(&frame("msg-1", 1, "sha256:aa"));
        assert_eq!(
            register.check_frame(&frame("msg-1", 1, "sha256:bb")),
            DedupVerdict::Conflict
        );
    }

    #[test]
    fn same_message_id_at_another_sequence_is_a_new_identity() {
        let mut register = DedupRegister::new();
        register.record_frame(&frame("msg-1", 1, "sha256:aa"));
        assert_eq!(
            register.check_frame(&frame("msg-1", 2, "sha256:aa")),
            DedupVerdict::New,
            "the frame identity is the messageId and sequence pair"
        );
    }

    #[test]
    fn command_idempotent_replay_is_a_duplicate() {
        let mut register = DedupRegister::new();
        let command = CommandIdentity::new("idem-1", "sha256:aa");
        assert_eq!(register.check_command(&command), DedupVerdict::New);
        register.record_command(&command);
        assert_eq!(register.check_command(&command), DedupVerdict::Duplicate);
    }

    #[test]
    fn command_key_reuse_with_a_different_payload_conflicts() {
        let mut register = DedupRegister::new();
        register.record_command(&CommandIdentity::new("idem-1", "sha256:aa"));
        assert_eq!(
            register.check_command(&CommandIdentity::new("idem-1", "sha256:bb")),
            DedupVerdict::Conflict
        );
    }

    #[test]
    fn compacting_frames_through_keeps_only_later_identities() {
        let mut register = DedupRegister::new();
        register.record_frame(&frame("msg-1", 1, "sha256:aa"));
        register.record_frame(&frame("msg-2", 2, "sha256:bb"));
        register.record_frame(&frame("msg-3", 3, "sha256:cc"));
        assert_eq!(register.compact_frames_through(2), 2);
        assert_eq!(register.retained_frames(), 1);
        assert_eq!(
            register.check_frame(&frame("msg-1", 1, "sha256:aa")),
            DedupVerdict::New,
            "compacted identities are forgotten"
        );
        assert_eq!(
            register.check_frame(&frame("msg-3", 3, "sha256:cc")),
            DedupVerdict::Duplicate
        );
    }

    #[test]
    fn command_records_survive_frame_compaction() {
        let mut register = DedupRegister::new();
        register.record_command(&CommandIdentity::new("idem-1", "sha256:aa"));
        register.compact_frames_through(9);
        assert_eq!(register.retained_commands(), 1);
        assert_eq!(
            register.check_command(&CommandIdentity::new("idem-1", "sha256:aa")),
            DedupVerdict::Duplicate
        );
    }
}
