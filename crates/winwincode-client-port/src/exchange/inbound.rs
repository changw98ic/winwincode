// SPDX-License-Identifier: Apache-2.0

//! Receive-side exchange state: instance guard, sequence validation,
//! contiguous acknowledgement advancement, and deduplication.
//!
//! One [`InboundStream`] covers one direction of one client node; both peers
//! run the same machinery. `ingest` returns the fixed contract outcomes for
//! one received frame: `duplicate`, `rejected_conflict`, `gap` with
//! `replayFromSequence`, `reacquire_required` on a changed
//! `clientInstanceId`, and acceptance that advances the contiguous
//! acknowledgement. The same-instance resume row of the contract
//! (`replay_required`) is expressed by the combination of the acknowledgement
//! cursor staying durable and the sender's outbox replaying everything the
//! peer has not confirmed yet.

use crate::exchange::dedup::CommandIdentity;
use crate::exchange::dedup::DedupRegister;
use crate::exchange::dedup::DedupVerdict;
use crate::exchange::frame::FrameIdentity;
use crate::exchange::sequence::AckCursor;
use crate::exchange::sequence::SequenceVerdict;

/// Command-level outcome attached to an accepted frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The command identity was unseen; the caller executes it.
    Fresh,
    /// The command identity was already accepted with the same payload
    /// digest; the caller must not execute it a second time.
    IdempotentReplay,
}

/// Fixed contract outcome for one received frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestDecision {
    /// The frame was accepted and the contiguous acknowledgement advanced.
    Accepted {
        /// The new highest consecutively accepted sequence.
        ack_sequence: u64,
        /// What the caller must do with an attached command.
        command: CommandOutcome,
    },
    /// The frame replayed an already accepted frame; confirm it without
    /// executing again.
    Duplicate {
        /// The unchanged contiguous acknowledgement.
        ack_sequence: u64,
    },
    /// A frame or command identity was reused with a different payload;
    /// accepted data is never overwritten and the acknowledgement does not
    /// move.
    RejectedConflict {
        /// The unchanged contiguous acknowledgement.
        ack_sequence: u64,
    },
    /// The frame was structurally invalid; nothing was recorded.
    RejectedMalformed {
        /// The unchanged contiguous acknowledgement.
        ack_sequence: u64,
    },
    /// A future sequence arrived; the acknowledgement stays and the sender
    /// must replay from `replay_from_sequence`.
    Gap {
        /// The unchanged contiguous acknowledgement.
        ack_sequence: u64,
        /// The first missing sequence (`replayFromSequence`).
        replay_from_sequence: u64,
    },
    /// The frame claims a `clientInstanceId` other than the current one; the
    /// old instance is superseded and its frames are refused.
    ReacquireRequired {
        /// The unchanged contiguous acknowledgement.
        ack_sequence: u64,
    },
}

impl IngestDecision {
    /// Returns the contiguous acknowledgement after the decision.
    #[must_use]
    pub const fn ack_sequence(&self) -> u64 {
        match *self {
            Self::Accepted { ack_sequence, .. }
            | Self::Duplicate { ack_sequence }
            | Self::RejectedConflict { ack_sequence }
            | Self::RejectedMalformed { ack_sequence }
            | Self::Gap { ack_sequence, .. }
            | Self::ReacquireRequired { ack_sequence } => ack_sequence,
        }
    }

    /// Returns the replay hint; only a gap carries one (`状态为 gap
    /// 时必须返回 replayFromSequence，其他状态不得附带该字段`).
    #[must_use]
    pub const fn replay_from_sequence(&self) -> Option<u64> {
        match *self {
            Self::Gap {
                replay_from_sequence,
                ..
            } => Some(replay_from_sequence),
            _ => None,
        }
    }

    /// Returns whether the decision accepted a new frame.
    #[must_use]
    pub const fn accepted(&self) -> bool {
        matches!(self, Self::Accepted { .. })
    }
}

/// Tracks the current `clientInstanceId` of one stream and classifies
/// instance changes.
///
/// The three verdicts encode the contract rows around `client.hello`: a
/// first instance joins, a same-instance resume replays unconfirmed frames
/// (`replay_required`), and an instance replacement supersedes the old one
/// (`reacquire_required`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstanceTracker {
    current: Option<String>,
}

/// Outcome of observing one `clientInstanceId`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceVerdict {
    /// No instance was recorded; this one becomes current.
    FirstInstance,
    /// The current instance re-appeared; both sides replay unconfirmed
    /// frames from their cursors.
    Resumed,
    /// A different instance appeared; the old instance is superseded and its
    /// commands and grants are refused from now on.
    Replaced {
        /// The instance that was replaced.
        superseded: String,
    },
}

impl InstanceTracker {
    /// Creates a tracker that has not seen an instance yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a tracker that already knows the current instance.
    #[must_use]
    pub fn with_initial(instance: impl Into<String>) -> Self {
        Self {
            current: Some(instance.into()),
        }
    }

    /// Returns the current instance, if one is recorded.
    #[must_use]
    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// Classifies one observed `clientInstanceId` and records the outcome.
    #[must_use]
    pub fn observe(&mut self, client_instance_id: &str) -> InstanceVerdict {
        match self.current.take() {
            None => {
                self.current = Some(client_instance_id.to_owned());
                InstanceVerdict::FirstInstance
            }
            Some(current) if current == client_instance_id => {
                self.current = Some(current);
                InstanceVerdict::Resumed
            }
            Some(superseded) => {
                self.current = Some(client_instance_id.to_owned());
                InstanceVerdict::Replaced { superseded }
            }
        }
    }
}

/// Receive-side state machine for one stream of one client node.
#[derive(Debug, Clone)]
pub struct InboundStream {
    instances: InstanceTracker,
    ack: AckCursor,
    dedup: DedupRegister,
    instance_guard: bool,
}

impl InboundStream {
    /// Creates a stream that learns the client instance from the first frame
    /// it accepts and refuses later instance changes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            instances: InstanceTracker::new(),
            ack: AckCursor::new(),
            dedup: DedupRegister::new(),
            instance_guard: true,
        }
    }

    /// Creates a stream bound to a known current client instance.
    #[must_use]
    pub fn with_instance(instance: impl Into<String>) -> Self {
        Self {
            instances: InstanceTracker::with_initial(instance),
            ..Self::new()
        }
    }

    /// Disables the instance guard for adapters that authenticate instances
    /// elsewhere.
    #[must_use]
    pub fn without_instance_guard(mut self) -> Self {
        self.instance_guard = false;
        self
    }

    /// Classifies one observed `clientInstanceId` (the `client.hello` path)
    /// and records the outcome.
    #[must_use]
    pub fn observe_instance(&mut self, client_instance_id: &str) -> InstanceVerdict {
        self.instances.observe(client_instance_id)
    }

    /// Returns the current client instance, if one is recorded.
    #[must_use]
    pub fn current_instance(&self) -> Option<&str> {
        self.instances.current()
    }

    /// Returns the highest consecutively accepted sequence
    /// (`ackSequence`).
    #[must_use]
    pub const fn ack_sequence(&self) -> u64 {
        self.ack.ack_sequence()
    }

    /// Returns the sequence the next accepted frame must carry.
    #[must_use]
    pub fn next_expected(&self) -> Option<u64> {
        self.ack.next_expected()
    }

    /// Returns how many frame identities are retained for deduplication.
    #[must_use]
    pub fn retained_dedup_frames(&self) -> usize {
        self.dedup.retained_frames()
    }

    /// Returns how many command identities are retained.
    #[must_use]
    pub fn retained_commands(&self) -> usize {
        self.dedup.retained_commands()
    }

    /// Forgets deduplicated frame identities covered by `sequence`; frames
    /// at or below a persisted acknowledgement are duplicates by sequence
    /// position regardless of the retained record.
    pub fn compact_dedup_through(&mut self, sequence: u64) -> usize {
        self.dedup.compact_frames_through(sequence)
    }

    /// Ingests one frame and returns the fixed contract outcome.
    ///
    /// `command` carries the `idempotencyKey` identity for command kinds;
    /// non-command facts (`report`/`ack`/`response`/`request`) pass `None`.
    ///
    /// Acceptance records both identities, advances the contiguous
    /// acknowledgement, and reports whether an attached command is fresh or
    /// an idempotent replay. Conflicts and gaps never advance the
    /// acknowledgement and never record state.
    #[allow(clippy::too_many_lines)]
    pub fn ingest(
        &mut self,
        client_instance_id: &str,
        identity: &FrameIdentity,
        command: Option<&CommandIdentity>,
    ) -> IngestDecision {
        let ack_sequence = self.ack.ack_sequence();
        if identity.sequence == 0
            || identity.message_id.is_empty()
            || identity.payload_digest.is_empty()
        {
            return IngestDecision::RejectedMalformed { ack_sequence };
        }

        if self.instance_guard {
            match self.instances.current() {
                None => {
                    let _ = self.instances.observe(client_instance_id);
                }
                Some(current) if current != client_instance_id => {
                    return IngestDecision::ReacquireRequired { ack_sequence };
                }
                Some(_) => {}
            }
        }

        match self.ack.observe(identity.sequence) {
            SequenceVerdict::Zero => IngestDecision::RejectedMalformed {
                ack_sequence: self.ack.ack_sequence(),
            },
            SequenceVerdict::Gap {
                replay_from_sequence,
            } => IngestDecision::Gap {
                ack_sequence,
                replay_from_sequence,
            },
            SequenceVerdict::Duplicate => {
                match self.dedup.check_frame(identity) {
                    DedupVerdict::Conflict => IngestDecision::RejectedConflict { ack_sequence },
                    // Frames at or below the contiguous acknowledgement are
                    // duplicates by sequence position even when the identity
                    // record was already compacted.
                    DedupVerdict::Duplicate | DedupVerdict::New => {
                        IngestDecision::Duplicate { ack_sequence }
                    }
                }
            }
            SequenceVerdict::Accept => {
                let command_outcome = match command {
                    None => CommandOutcome::Fresh,
                    Some(command) => match self.dedup.check_command(command) {
                        DedupVerdict::Conflict => {
                            return IngestDecision::RejectedConflict { ack_sequence };
                        }
                        DedupVerdict::Duplicate => CommandOutcome::IdempotentReplay,
                        DedupVerdict::New => CommandOutcome::Fresh,
                    },
                };
                if self.dedup.check_frame(identity) == DedupVerdict::Conflict {
                    return IngestDecision::RejectedConflict { ack_sequence };
                }

                self.dedup.record_frame(identity);
                if let Some(command) = command {
                    self.dedup.record_command(command);
                }
                let advanced = self.ack.advance(identity.sequence);
                debug_assert!(
                    advanced.is_ok(),
                    "observe returned Accept on the same cursor, so the contiguous advance holds"
                );
                IngestDecision::Accepted {
                    ack_sequence: self.ack.ack_sequence(),
                    command: command_outcome,
                }
            }
        }
    }
}

impl Default for InboundStream {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(sequence: u64) -> FrameIdentity {
        FrameIdentity::new(
            format!("msg-{sequence}"),
            sequence,
            format!("sha256:{sequence:02x}"),
        )
    }

    #[test]
    fn consecutive_frames_advance_the_acknowledgement() {
        let mut stream = InboundStream::with_instance("inst-1");
        let first = stream.ingest("inst-1", &identity(1), None);
        assert_eq!(
            first,
            IngestDecision::Accepted {
                ack_sequence: 1,
                command: CommandOutcome::Fresh
            }
        );
        assert_eq!(
            stream.ingest("inst-1", &identity(2), None).ack_sequence(),
            2
        );
        assert_eq!(
            stream.ingest("inst-1", &identity(3), None).ack_sequence(),
            3
        );
        assert_eq!(stream.ack_sequence(), 3);
        assert_eq!(stream.next_expected(), Some(4));
    }

    #[test]
    fn a_future_sequence_is_a_gap_with_a_replay_hint() {
        let mut stream = InboundStream::with_instance("inst-1");
        stream.ingest("inst-1", &identity(1), None);
        stream.ingest("inst-1", &identity(2), None);
        let decision = stream.ingest("inst-1", &identity(5), None);
        assert_eq!(
            decision,
            IngestDecision::Gap {
                ack_sequence: 2,
                replay_from_sequence: 3
            }
        );
        assert_eq!(decision.replay_from_sequence(), Some(3));
        assert_eq!(
            stream.ack_sequence(),
            2,
            "a gap must not move the acknowledgement"
        );
    }

    #[test]
    fn the_replayed_frame_closes_the_gap() {
        let mut stream = InboundStream::with_instance("inst-1");
        stream.ingest("inst-1", &identity(1), None);
        assert_eq!(
            stream.ingest("inst-1", &identity(3), None),
            IngestDecision::Gap {
                ack_sequence: 1,
                replay_from_sequence: 2
            }
        );
        // The sender replays from `replayFromSequence`; filling the gap
        // accepts the buffered sequence too.
        assert_eq!(
            stream.ingest("inst-1", &identity(2), None),
            IngestDecision::Accepted {
                ack_sequence: 2,
                command: CommandOutcome::Fresh
            }
        );
        assert_eq!(
            stream.ingest("inst-1", &identity(3), None),
            IngestDecision::Accepted {
                ack_sequence: 3,
                command: CommandOutcome::Fresh
            }
        );
    }

    #[test]
    fn an_exact_replay_below_the_cursor_is_a_duplicate() {
        let mut stream = InboundStream::with_instance("inst-1");
        stream.ingest("inst-1", &identity(1), None);
        stream.ingest("inst-1", &identity(2), None);
        let replayed = stream.ingest("inst-1", &identity(1), None);
        assert_eq!(replayed, IngestDecision::Duplicate { ack_sequence: 2 });
        assert_eq!(replayed.replay_from_sequence(), None);
        assert_eq!(stream.ack_sequence(), 2);
    }

    #[test]
    fn a_below_cursor_replay_stays_a_duplicate_after_compaction() {
        let mut stream = InboundStream::with_instance("inst-1");
        stream.ingest("inst-1", &identity(1), None);
        stream.ingest("inst-1", &identity(2), None);
        assert_eq!(stream.compact_dedup_through(1), 1);
        assert_eq!(
            stream.ingest("inst-1", &identity(1), None),
            IngestDecision::Duplicate { ack_sequence: 2 }
        );
    }

    #[test]
    fn a_retained_identity_with_a_different_digest_conflicts() {
        let mut stream = InboundStream::with_instance("inst-1");
        stream.ingest("inst-1", &identity(1), None);
        stream.ingest("inst-1", &identity(2), None);
        // The frame identity (messageId, sequence) is retained with its
        // accepted digest; the same identity with a different payload digest
        // is refused instead of being silently confirmed.
        let tampered = FrameIdentity::new("msg-2", 2, "sha256:dead");
        assert_eq!(
            stream.ingest("inst-1", &tampered, None),
            IngestDecision::RejectedConflict { ack_sequence: 2 }
        );
        // Once the record is compacted away, the sequence position decides.
        stream.compact_dedup_through(2);
        assert_eq!(
            stream.ingest("inst-1", &tampered, None),
            IngestDecision::Duplicate { ack_sequence: 2 }
        );
    }

    #[test]
    fn malformed_frames_are_rejected_without_state_changes() {
        let mut stream = InboundStream::with_instance("inst-1");
        stream.ingest("inst-1", &identity(1), None);
        for malformed in [
            FrameIdentity::new("msg-0", 0, "sha256:00"),
            FrameIdentity::new("", 2, "sha256:02"),
            FrameIdentity::new("msg-2", 2, ""),
        ] {
            assert_eq!(
                stream.ingest("inst-1", &malformed, None),
                IngestDecision::RejectedMalformed { ack_sequence: 1 }
            );
        }
        assert_eq!(stream.ack_sequence(), 1);
    }

    #[test]
    fn a_command_key_reuse_with_a_different_payload_conflicts() {
        let mut stream = InboundStream::with_instance("inst-1");
        let command = CommandIdentity::new("idem-1", "sha256:aa");
        stream.ingest("inst-1", &identity(1), Some(&command));
        let conflicting_command = CommandIdentity::new("idem-1", "sha256:bb");
        let decision = stream.ingest("inst-1", &identity(2), Some(&conflicting_command));
        assert_eq!(
            decision,
            IngestDecision::RejectedConflict { ack_sequence: 1 }
        );
        assert_eq!(
            stream.ack_sequence(),
            1,
            "conflicts never advance the acknowledgement"
        );
        assert_eq!(
            stream.retained_dedup_frames(),
            1,
            "conflicts never record the rejected frame"
        );
    }

    #[test]
    fn an_idempotent_command_replay_advances_without_a_second_execution() {
        let mut stream = InboundStream::with_instance("inst-1");
        let command = CommandIdentity::new("idem-1", "sha256:aa");
        assert!(matches!(
            stream.ingest("inst-1", &identity(1), Some(&command)),
            IngestDecision::Accepted {
                command: CommandOutcome::Fresh,
                ..
            }
        ));
        // The sender rebuilt the frame at the next sequence but reused the
        // key with the same payload: accept the frame, but do not re-execute.
        let rebuilt = FrameIdentity::new("msg-2", 2, "sha256:aa");
        assert_eq!(
            stream.ingest("inst-1", &rebuilt, Some(&command)),
            IngestDecision::Accepted {
                ack_sequence: 2,
                command: CommandOutcome::IdempotentReplay
            }
        );
    }

    #[test]
    fn an_instance_change_requires_reacquire() {
        let mut stream = InboundStream::with_instance("inst-1");
        stream.ingest("inst-1", &identity(1), None);
        assert_eq!(
            stream.ingest("inst-2", &identity(2), None),
            IngestDecision::ReacquireRequired { ack_sequence: 1 }
        );
        assert_eq!(stream.ack_sequence(), 1);
        assert!(
            !stream.ingest("inst-2", &identity(2), None).accepted(),
            "the superseded request keeps being refused until the instance is re-observed"
        );
        assert_eq!(
            stream.observe_instance("inst-2"),
            InstanceVerdict::Replaced {
                superseded: "inst-1".to_owned()
            }
        );
        assert_eq!(
            stream.ingest("inst-2", &identity(2), None),
            IngestDecision::Accepted {
                ack_sequence: 2,
                command: CommandOutcome::Fresh
            },
            "the new instance continues the same per-node stream"
        );
    }

    #[test]
    fn the_first_frame_binds_an_unbound_stream() {
        let mut stream = InboundStream::new();
        assert_eq!(stream.current_instance(), None);
        assert!(stream.ingest("inst-9", &identity(1), None).accepted());
        assert_eq!(stream.current_instance(), Some("inst-9"));
        assert_eq!(
            stream.ingest("inst-8", &identity(2), None),
            IngestDecision::ReacquireRequired { ack_sequence: 1 }
        );
    }

    #[test]
    fn a_unguarded_stream_accepts_any_instance() {
        let mut stream = InboundStream::new().without_instance_guard();
        assert!(stream.ingest("inst-1", &identity(1), None).accepted());
        assert!(stream.ingest("inst-2", &identity(2), None).accepted());
    }

    #[test]
    fn instance_tracker_classifies_hello_outcomes() {
        let mut tracker = InstanceTracker::new();
        assert_eq!(tracker.observe("inst-1"), InstanceVerdict::FirstInstance);
        assert_eq!(tracker.observe("inst-1"), InstanceVerdict::Resumed);
        assert_eq!(
            tracker.observe("inst-2"),
            InstanceVerdict::Replaced {
                superseded: "inst-1".to_owned()
            }
        );
        assert_eq!(tracker.current(), Some("inst-2"));
    }

    #[test]
    fn a_tracker_with_an_initial_instance_resumes() {
        let mut tracker = InstanceTracker::with_initial("inst-1");
        assert_eq!(tracker.current(), Some("inst-1"));
        assert_eq!(tracker.observe("inst-1"), InstanceVerdict::Resumed);
    }

    #[test]
    fn only_gaps_carry_a_replay_hint() {
        assert_eq!(
            IngestDecision::Duplicate { ack_sequence: 4 }.replay_from_sequence(),
            None
        );
        assert_eq!(
            IngestDecision::Accepted {
                ack_sequence: 4,
                command: CommandOutcome::Fresh
            }
            .replay_from_sequence(),
            None
        );
        assert_eq!(
            IngestDecision::Gap {
                ack_sequence: 4,
                replay_from_sequence: 5
            }
            .replay_from_sequence(),
            Some(5)
        );
    }

    #[test]
    fn compaction_drops_only_confirmed_identities() {
        let mut stream = InboundStream::with_instance("inst-1");
        for sequence in 1..=3 {
            stream.ingest("inst-1", &identity(sequence), None);
        }
        assert_eq!(stream.retained_dedup_frames(), 3);
        assert_eq!(stream.compact_dedup_through(2), 2);
        assert_eq!(stream.retained_dedup_frames(), 1);
        assert_eq!(stream.retained_commands(), 0);
    }
}
