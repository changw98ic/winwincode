// SPDX-License-Identifier: Apache-2.0

//! Bidirectional sequence allocation and acknowledgement cursors.
//!
//! Both directions of one exchange number frames from 1 with no gaps
//! (`两个流各自从 1 开始连续递增`). The sender side allocates sequences through
//! [`SequenceAllocator`] immediately before the durable outbox append; the
//! receiver side tracks the contiguous acknowledgement through [`AckCursor`].
//!
//! `ackSequence` means the highest consecutively accepted sequence, never the
//! highest sequence seen (`ackSequence 表示接收方已连续接受的最大 sequence，
//! 而不是“见过的最大值”`). A sequence beyond the cursor is a gap: the
//! acknowledgement stays where it is and the receiver returns
//! `replayFromSequence` pointing at the first missing frame.

/// Sequence numbers of one exchange stream.
pub type Sequence = u64;

/// Sender-side sequence allocation for one stream.
///
/// The allocator is the in-memory cursor; the durable outbox append is the
/// crash-safe boundary (`先持久化再发送`), so a sender restores the allocator
/// from the outbox cursor after a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceAllocator {
    next: Sequence,
}

impl SequenceAllocator {
    /// Creates an allocator whose first sequence is 1.
    #[must_use]
    pub const fn new() -> Self {
        Self { next: 1 }
    }

    /// Restores an allocator from the durable outbox cursor. `next` is the
    /// sequence the next appended frame must carry.
    ///
    /// # Errors
    ///
    /// Fails when `next` is zero; sequences start at 1.
    pub const fn from_next(next: Sequence) -> Result<Self, SequenceError> {
        if next == 0 {
            return Err(SequenceError::ZeroSequence);
        }
        Ok(Self { next })
    }

    /// Returns the sequence the next allocation will hand out.
    #[must_use]
    pub const fn peek_next(&self) -> Sequence {
        self.next
    }

    /// Allocates the next sequence of the stream.
    ///
    /// # Errors
    ///
    /// Fails when the sequence space is exhausted at [`u64::MAX`].
    pub const fn allocate(&mut self) -> Result<Sequence, SequenceError> {
        let allocated = self.next;
        match self.next.checked_add(1) {
            Some(next) => {
                self.next = next;
                Ok(allocated)
            }
            None => Err(SequenceError::Overflow),
        }
    }
}

impl Default for SequenceAllocator {
    fn default() -> Self {
        Self::new()
    }
}

/// Sequence allocation failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceError {
    /// Sequence zero does not exist; sequences start at 1.
    ZeroSequence,
    /// The sequence space is exhausted at [`u64::MAX`].
    Overflow,
}

/// Receiver-side contiguous acknowledgement cursor for one stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckCursor {
    ack_sequence: Sequence,
}

/// Result of observing one inbound sequence against an [`AckCursor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceVerdict {
    /// The sequence is exactly the cursor's next expected frame.
    Accept,
    /// The sequence is at or below the contiguous acknowledgement; it was
    /// already accepted.
    Duplicate,
    /// The sequence is beyond the next expected frame; the receiver keeps its
    /// acknowledgement and asks for a replay from the first missing sequence.
    Gap {
        /// The first sequence the sender must replay (`replayFromSequence`).
        replay_from_sequence: Sequence,
    },
    /// Sequence zero does not exist; sequences start at 1.
    Zero,
}

/// Contiguous acknowledgement advancement failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckAdvanceError {
    /// The cursor is saturated at [`u64::MAX`] and cannot advance further.
    Overflow,
    /// Only the consecutive next sequence may advance the cursor
    /// (`ack 推进只认连续`).
    NotContiguous {
        /// The current acknowledgement.
        ack_sequence: Sequence,
        /// The rejected sequence.
        sequence: Sequence,
    },
}

impl AckCursor {
    /// Creates a cursor that has accepted nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { ack_sequence: 0 }
    }

    /// Restores a cursor from a persisted acknowledgement.
    #[must_use]
    pub const fn from_ack(ack_sequence: Sequence) -> Self {
        Self { ack_sequence }
    }

    /// Returns the highest consecutively accepted sequence.
    #[must_use]
    pub const fn ack_sequence(&self) -> Sequence {
        self.ack_sequence
    }

    /// Returns the sequence the next accepted frame must carry, or `None`
    /// once the cursor saturated at [`u64::MAX`].
    #[must_use]
    pub const fn next_expected(&self) -> Option<Sequence> {
        self.ack_sequence.checked_add(1)
    }

    /// Classifies one inbound sequence without mutating the cursor.
    #[must_use]
    pub fn observe(&self, sequence: Sequence) -> SequenceVerdict {
        if sequence == 0 {
            return SequenceVerdict::Zero;
        }
        match self.next_expected() {
            Some(expected) if sequence == expected => SequenceVerdict::Accept,
            Some(expected) if sequence > expected => SequenceVerdict::Gap {
                replay_from_sequence: expected,
            },
            // Below the cursor, or the cursor saturated at u64::MAX.
            None | Some(_) => SequenceVerdict::Duplicate,
        }
    }

    /// Advances the contiguous acknowledgement by exactly one frame.
    ///
    /// # Errors
    ///
    /// Fails when `sequence` is not the consecutive next sequence or the
    /// cursor saturated at [`u64::MAX`].
    pub const fn advance(&mut self, sequence: Sequence) -> Result<(), AckAdvanceError> {
        match self.next_expected() {
            None => Err(AckAdvanceError::Overflow),
            Some(expected) if sequence == expected => {
                self.ack_sequence = sequence;
                Ok(())
            }
            Some(_) => Err(AckAdvanceError::NotContiguous {
                ack_sequence: self.ack_sequence,
                sequence,
            }),
        }
    }
}

impl Default for AckCursor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_starts_at_one_and_counts() {
        let mut allocator = SequenceAllocator::new();
        assert_eq!(allocator.peek_next(), 1);
        assert_eq!(allocator.allocate(), Ok(1));
        assert_eq!(allocator.allocate(), Ok(2));
        assert_eq!(allocator.allocate(), Ok(3));
        assert_eq!(allocator.peek_next(), 4);
    }

    #[test]
    fn allocator_from_next_restores_and_rejects_zero() {
        let mut restored = SequenceAllocator::from_next(41).expect("restore allocator");
        assert_eq!(restored.allocate(), Ok(41));
        assert_eq!(
            SequenceAllocator::from_next(0),
            Err(SequenceError::ZeroSequence)
        );
    }

    #[test]
    fn allocator_reports_exhausted_sequence_space() {
        let mut allocator = SequenceAllocator::from_next(u64::MAX).expect("restore allocator");
        assert_eq!(allocator.allocate(), Err(SequenceError::Overflow));
        assert_eq!(
            allocator.peek_next(),
            u64::MAX,
            "a failed allocation must not move the cursor"
        );
    }

    #[test]
    fn cursor_accepts_only_the_consecutive_next_sequence() {
        let mut cursor = AckCursor::new();
        assert_eq!(cursor.observe(1), SequenceVerdict::Accept);
        assert!(cursor.advance(1).is_ok());
        assert_eq!(cursor.ack_sequence(), 1);
        assert_eq!(cursor.observe(2), SequenceVerdict::Accept);
        assert!(cursor.advance(2).is_ok());
        assert_eq!(cursor.ack_sequence(), 2);
        assert_eq!(cursor.next_expected(), Some(3));
    }

    #[test]
    fn cursor_treats_below_cursor_sequences_as_duplicate() {
        let cursor = AckCursor::from_ack(3);
        assert_eq!(cursor.observe(1), SequenceVerdict::Duplicate);
        assert_eq!(cursor.observe(3), SequenceVerdict::Duplicate);
    }

    #[test]
    fn cursor_beyond_next_is_a_gap_pointing_at_the_first_missing_frame() {
        let cursor = AckCursor::from_ack(3);
        assert_eq!(
            cursor.observe(5),
            SequenceVerdict::Gap {
                replay_from_sequence: 4
            }
        );
        assert_eq!(
            cursor.ack_sequence(),
            3,
            "observing a gap must not move the cursor"
        );
    }

    #[test]
    fn cursor_rejects_sequence_zero() {
        let cursor = AckCursor::new();
        assert_eq!(cursor.observe(0), SequenceVerdict::Zero);
    }

    #[test]
    fn advance_rejects_nonconsecutive_sequences_without_moving() {
        let mut cursor = AckCursor::from_ack(3);
        assert_eq!(
            cursor.advance(7),
            Err(AckAdvanceError::NotContiguous {
                ack_sequence: 3,
                sequence: 7
            })
        );
        assert_eq!(cursor.ack_sequence(), 3);
    }

    #[test]
    fn advance_reports_overflow_at_the_top_of_the_sequence_space() {
        let mut cursor = AckCursor::from_ack(u64::MAX);
        assert_eq!(cursor.next_expected(), None);
        assert_eq!(cursor.advance(0), Err(AckAdvanceError::Overflow));
    }

    #[test]
    fn saturated_cursor_treats_every_nonzero_sequence_as_duplicate() {
        let cursor = AckCursor::from_ack(u64::MAX);
        assert_eq!(cursor.observe(u64::MAX), SequenceVerdict::Duplicate);
        assert_eq!(cursor.observe(1), SequenceVerdict::Duplicate);
    }

    #[test]
    fn cursor_restores_from_a_persisted_acknowledgement() {
        let restored = AckCursor::from_ack(12);
        assert_eq!(restored.ack_sequence(), 12);
        assert_eq!(restored.next_expected(), Some(13));
    }
}
