// SPDX-License-Identifier: Apache-2.0

pub type Sequence = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceAllocator {
    next: Sequence,
}

impl SequenceAllocator {
    #[must_use]
    pub const fn new() -> Self {
        Self { next: 1 }
    }

    pub const fn from_next(next: Sequence) -> Result<Self, SequenceError> {
        if next == 0 {
            return Err(SequenceError::ZeroSequence);
        }
        Ok(Self { next })
    }

    #[must_use]
    pub const fn peek_next(&self) -> Sequence {
        self.next
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceError {
    ZeroSequence,
    Overflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckCursor {
    ack_sequence: Sequence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceVerdict {
    Accept,
    Duplicate,
    Gap { replay_from_sequence: Sequence },
    Zero,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckAdvanceError {
    Overflow,
    NotContiguous {
        ack_sequence: Sequence,
        sequence: Sequence,
    },
}

impl AckCursor {
    #[must_use]
    pub const fn new() -> Self {
        Self { ack_sequence: 0 }
    }

    #[must_use]
    pub const fn from_ack(ack_sequence: Sequence) -> Self {
        Self { ack_sequence }
    }

    #[must_use]
    pub const fn ack_sequence(&self) -> Sequence {
        self.ack_sequence
    }

    #[must_use]
    pub const fn next_expected(&self) -> Option<Sequence> {
        self.ack_sequence.checked_add(1)
    }

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
            None | Some(_) => SequenceVerdict::Duplicate,
        }
    }

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
