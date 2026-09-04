// SPDX-License-Identifier: Apache-2.0

//! The durable sender-outbox seam and its in-memory reference
//! implementation.
//!
//! One outbox owns one sender stream: the frames retained for delivery and
//! the durable acknowledgement cursor of the peer. `append` is the atomic
//! persist-before-send boundary (`先持久化再发送`); `record_acknowledgement`
//! persists the peer's contiguous acknowledgement so confirmed frames may
//! then be compacted (`发送方在持久化 ackSequence 后可以压缩已确认 frame`).
//!
//! `SQLite` and other durable adapters implement [`FrameOutbox`] in their own
//! lanes; this module ships the trait plus the in-memory reference
//! implementation. The state machine ([`OutboxSession`]) validates every
//! snapshot it loads: a retained range with a missing contiguous frame is a
//! corrupt state and recovery is refused (`缺失即按损坏状态拒绝恢复`).

use std::collections::HashSet;

use crate::exchange::frame::StoredFrame;

/// Durable rows of one sender stream, loaded from the outbox adapter.
///
/// Frames are contiguous and ordered. Retained frames may start after
/// sequence one only when every omitted prefix frame is covered by
/// `ack_sequence`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxSnapshot {
    /// Highest sequence the receiving peer confirmed as accepted.
    pub ack_sequence: u64,
    /// Highest contiguous sequence retained (or already compacted) by the
    /// sender. This is a durable high-water mark, not the peer's
    /// acknowledgement.
    pub highest_sequence: u64,
    /// Retained original frames, ordered by sequence.
    pub frames: Vec<StoredFrame>,
}

/// Validation failures found in a durable outbox snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxStateError {
    /// Sequence zero does not exist; sequences start at 1.
    ZeroSequence,
    /// A retained frame carries no message id.
    EmptyMessageId,
    /// A retained frame carries no payload digest.
    EmptyDigest,
    /// A retained frame carries no encoded bytes.
    EmptyFrame,
    /// The same message id was retained twice.
    DuplicateMessageId(String),
    /// The retained frames are not contiguous.
    NonContiguous {
        /// The sequence the stream required.
        expected: u64,
        /// The sequence the snapshot carried.
        found: u64,
    },
    /// The stored high-water mark disagrees with the retained frames.
    HighestMismatch {
        /// The persisted high-water mark.
        stored: u64,
        /// The high-water mark derived from the frames.
        derived: u64,
    },
    /// The acknowledgement exceeds the retained high-water mark.
    AckExceedsHighest {
        /// The persisted acknowledgement.
        acknowledged: u64,
        /// The persisted high-water mark.
        highest: u64,
    },
    /// The sequence space is exhausted at [`u64::MAX`].
    SequenceOverflow,
}

impl OutboxSnapshot {
    /// Checks the invariants a durable adapter must preserve.
    ///
    /// # Errors
    ///
    /// Returns the precise invariant violation when retained frames carry
    /// invalid fields, are duplicated or non-contiguous, or disagree with the
    /// stored acknowledgement or high-water mark.
    pub fn validate(&self) -> Result<(), OutboxStateError> {
        if self.ack_sequence > self.highest_sequence {
            return Err(OutboxStateError::AckExceedsHighest {
                acknowledged: self.ack_sequence,
                highest: self.highest_sequence,
            });
        }

        let first_sequence = self.frames.first().map_or(1, |frame| frame.sequence);
        let mut expected = first_sequence;
        let mut message_ids = HashSet::with_capacity(self.frames.len());

        for frame in &self.frames {
            if frame.sequence == 0 {
                return Err(OutboxStateError::ZeroSequence);
            }
            if frame.message_id.is_empty() {
                return Err(OutboxStateError::EmptyMessageId);
            }
            if frame.payload_digest.is_empty() {
                return Err(OutboxStateError::EmptyDigest);
            }
            if frame.frame.is_empty() {
                return Err(OutboxStateError::EmptyFrame);
            }
            if !message_ids.insert(frame.message_id.clone()) {
                return Err(OutboxStateError::DuplicateMessageId(
                    frame.message_id.clone(),
                ));
            }
            if frame.sequence != expected {
                return Err(OutboxStateError::NonContiguous {
                    expected,
                    found: frame.sequence,
                });
            }
            expected = expected
                .checked_add(1)
                .ok_or(OutboxStateError::SequenceOverflow)?;
        }

        if self
            .ack_sequence
            .checked_add(1)
            .is_some_and(|first_unacknowledged| first_sequence > first_unacknowledged)
        {
            return Err(OutboxStateError::NonContiguous {
                expected: self.ack_sequence.saturating_add(1),
                found: first_sequence,
            });
        }

        let derived_highest = self
            .frames
            .last()
            .map_or(self.ack_sequence, |frame| frame.sequence);
        if self.highest_sequence != derived_highest {
            return Err(OutboxStateError::HighestMismatch {
                stored: self.highest_sequence,
                derived: derived_highest,
            });
        }
        Ok(())
    }
}

/// Durable persistence seam for one sender stream.
///
/// `append` must atomically verify `expected_highest_sequence`, persist
/// `frame`, and advance the retained high-water mark to
/// `expected_highest_sequence + 1`: a `SQLite` adapter does this in one
/// transaction. The state machine never treats its local snapshot as durable
/// truth.
pub trait FrameOutbox {
    /// Persistence error of the adapter.
    type Error;

    /// Loads the complete durable snapshot of the stream, or `None` when the
    /// stream has no rows yet.
    ///
    /// # Errors
    ///
    /// Returns the persistence implementation's read error.
    fn load(&mut self) -> Result<Option<OutboxSnapshot>, Self::Error>;

    /// Atomically persists one frame when the durable high-water mark still
    /// equals `expected_highest_sequence`.
    ///
    /// # Errors
    ///
    /// Returns the persistence implementation's transaction or race error.
    fn append(
        &mut self,
        expected_highest_sequence: u64,
        frame: &StoredFrame,
    ) -> Result<(), Self::Error>;

    /// Atomically advances the peer acknowledgement watermark.
    ///
    /// # Errors
    ///
    /// Returns the adapter's transaction or acknowledgement-race error.
    fn record_acknowledgement(
        &mut self,
        expected_ack_sequence: u64,
        ack_sequence: u64,
    ) -> Result<(), Self::Error>;
}

/// Optional compaction capability of an outbox adapter.
///
/// Compaction drops frames the persisted acknowledgement already covers; the
/// acknowledgement cursor and the high-water mark must survive it.
pub trait CompactingOutbox: FrameOutbox {
    /// Drops retained frames at or below `ack_sequence` and returns how many
    /// frames were dropped.
    ///
    /// # Errors
    ///
    /// Returns the persistence implementation's transaction error.
    fn compact_through(&mut self, ack_sequence: u64) -> Result<usize, Self::Error>;
}

/// Invalid caller input detected before any durable mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxInputError {
    /// A frame carries no message id.
    EmptyMessageId,
    /// A frame carries sequence zero.
    ZeroSequence,
    /// A frame carries no payload digest.
    EmptyDigest,
    /// A frame carries no encoded bytes.
    EmptyFrame,
    /// The message id is already retained by the stream.
    DuplicateMessageId(String),
    /// A delivery batch requested zero frames.
    ZeroMaxFrames,
}

/// Failure from persistence, invalid input, a corrupt snapshot, or a rejected
/// cursor.
#[derive(Debug, PartialEq, Eq)]
pub enum OutboxError<E> {
    /// The adapter failed.
    Store(E),
    /// Caller input was invalid.
    InvalidInput(OutboxInputError),
    /// The loaded durable snapshot violated its invariants.
    CorruptState(OutboxStateError),
    /// The frame did not carry the sequence the stream required.
    SequenceMismatch {
        /// The sequence the stream required.
        expected: u64,
        /// The sequence the frame carried.
        found: u64,
    },
    /// The sequence space is exhausted at [`u64::MAX`].
    SequenceExhausted,
    /// The requested delivery cursor is beyond the retained high-water mark.
    CursorAhead {
        /// The requested cursor.
        requested: u64,
        /// The retained high-water mark.
        highest: u64,
    },
    /// The peer acknowledged more than the sender ever retained.
    AckAhead {
        /// The requested acknowledgement.
        requested: u64,
        /// The retained high-water mark.
        highest: u64,
    },
    /// The peer acknowledgement moved backwards.
    AckRegression {
        /// The requested acknowledgement.
        requested: u64,
        /// The currently persisted acknowledgement.
        acknowledged: u64,
    },
}

/// One bounded delivery batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxBatch {
    /// Highest sequence the peer has acknowledged so far.
    pub ack_sequence: u64,
    /// Highest contiguous sequence retained by the sender.
    pub highest_sequence: u64,
    /// The frames to deliver; the remainder stays for later exchanges.
    pub frames: Vec<StoredFrame>,
}

/// Stateless outbox state machine backed by a caller-provided adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutboxSession;

impl OutboxSession {
    /// Creates a state machine. All accepted state remains in the adapter.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Returns the sequence the next appended frame must carry.
    ///
    /// # Errors
    ///
    /// Returns an adapter error, a corrupt snapshot, or sequence-space
    /// exhaustion at [`u64::MAX`].
    pub fn next_sequence<S>(&self, store: &mut S) -> Result<u64, OutboxError<S::Error>>
    where
        S: FrameOutbox,
    {
        let snapshot = snapshot_of(store)?;
        snapshot
            .highest_sequence
            .checked_add(1)
            .ok_or(OutboxError::SequenceExhausted)
    }

    /// Persists one frame at the sequence [`OutboxSession::next_sequence`]
    /// handed out (persist-before-send) and returns the frame's sequence.
    ///
    /// The adapter receives the compare-and-append base internally, so
    /// callers only deal with the sequence the next frame must carry.
    ///
    /// # Errors
    ///
    /// Returns invalid frame input, a sequence that disagrees with the
    /// caller's expectation or the stream's cursor, an adapter error, or a
    /// corrupt snapshot.
    pub fn enqueue<S>(
        &self,
        store: &mut S,
        expected_next_sequence: u64,
        frame: &StoredFrame,
    ) -> Result<u64, OutboxError<S::Error>>
    where
        S: FrameOutbox,
    {
        validate_frame_shape(frame).map_err(OutboxError::InvalidInput)?;
        if frame.sequence != expected_next_sequence {
            return Err(OutboxError::SequenceMismatch {
                expected: expected_next_sequence,
                found: frame.sequence,
            });
        }
        let snapshot = snapshot_of(store)?;
        let stream_expected = snapshot
            .highest_sequence
            .checked_add(1)
            .ok_or(OutboxError::SequenceExhausted)?;
        if frame.sequence != stream_expected {
            return Err(OutboxError::SequenceMismatch {
                expected: stream_expected,
                found: frame.sequence,
            });
        }
        if snapshot
            .frames
            .iter()
            .any(|retained| retained.message_id == frame.message_id)
        {
            return Err(OutboxError::InvalidInput(
                OutboxInputError::DuplicateMessageId(frame.message_id.clone()),
            ));
        }

        let expected_highest_sequence = frame.sequence - 1;
        store
            .append(expected_highest_sequence, frame)
            .map_err(OutboxError::Store)?;
        Ok(frame.sequence)
    }

    /// Returns at most `max_frames` frames strictly after `after_sequence`.
    /// Undelivered remainders stay in the outbox for later exchanges.
    ///
    /// # Errors
    ///
    /// Returns a zero page size, a cursor beyond the retained high-water
    /// mark, an adapter error, or a corrupt snapshot.
    pub fn deliverable<S>(
        &self,
        store: &mut S,
        after_sequence: u64,
        max_frames: usize,
    ) -> Result<OutboxBatch, OutboxError<S::Error>>
    where
        S: FrameOutbox,
    {
        if max_frames == 0 {
            return Err(OutboxError::InvalidInput(OutboxInputError::ZeroMaxFrames));
        }
        let snapshot = snapshot_of(store)?;
        if after_sequence > snapshot.highest_sequence {
            return Err(OutboxError::CursorAhead {
                requested: after_sequence,
                highest: snapshot.highest_sequence,
            });
        }
        let frames = snapshot
            .frames
            .iter()
            .filter(|frame| frame.sequence > after_sequence)
            .take(max_frames)
            .cloned()
            .collect();
        Ok(OutboxBatch {
            ack_sequence: snapshot.ack_sequence,
            highest_sequence: snapshot.highest_sequence,
            frames,
        })
    }

    /// Records the peer's contiguous acknowledgement watermark without
    /// changing the retained frames.
    ///
    /// # Errors
    ///
    /// Returns a watermark ahead of the retained range, a regressing
    /// watermark, an adapter error, or a corrupt snapshot.
    pub fn acknowledge<S>(
        &self,
        store: &mut S,
        ack_sequence: u64,
    ) -> Result<u64, OutboxError<S::Error>>
    where
        S: FrameOutbox,
    {
        let snapshot = snapshot_of(store)?;
        if ack_sequence > snapshot.highest_sequence {
            return Err(OutboxError::AckAhead {
                requested: ack_sequence,
                highest: snapshot.highest_sequence,
            });
        }
        if ack_sequence < snapshot.ack_sequence {
            return Err(OutboxError::AckRegression {
                requested: ack_sequence,
                acknowledged: snapshot.ack_sequence,
            });
        }
        if ack_sequence == snapshot.ack_sequence {
            return Ok(ack_sequence);
        }
        store
            .record_acknowledgement(snapshot.ack_sequence, ack_sequence)
            .map_err(OutboxError::Store)?;
        Ok(ack_sequence)
    }

    /// Drops frames the persisted acknowledgement already covers and returns
    /// how many frames were dropped.
    ///
    /// # Errors
    ///
    /// Returns an adapter error or a corrupt snapshot.
    pub fn compact_confirmed<C>(&self, store: &mut C) -> Result<usize, OutboxError<C::Error>>
    where
        C: CompactingOutbox,
    {
        let snapshot = snapshot_of(store)?;
        store
            .compact_through(snapshot.ack_sequence)
            .map_err(OutboxError::Store)
    }
}

/// In-memory reference implementation of [`FrameOutbox`].
///
/// It trusts the state machine's pre-validation and keeps the whole snapshot
/// in memory; durable adapters must enforce the compare-and-append and
/// acknowledgement watermarks atomically themselves.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InMemoryOutbox {
    state: Option<OutboxSnapshot>,
}

impl InMemoryOutbox {
    /// Creates an empty outbox.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds an outbox from a previously persisted snapshot.
    ///
    /// The snapshot is kept verbatim so a corrupt persisted state surfaces
    /// through the state machine's validation instead of being silently
    /// repaired here.
    #[must_use]
    pub fn from_snapshot(snapshot: OutboxSnapshot) -> Self {
        Self {
            state: Some(snapshot),
        }
    }

    /// Returns the current snapshot, if the stream has rows.
    #[must_use]
    pub fn snapshot(&self) -> Option<&OutboxSnapshot> {
        self.state.as_ref()
    }
}

impl FrameOutbox for InMemoryOutbox {
    type Error = core::convert::Infallible;

    fn load(&mut self) -> Result<Option<OutboxSnapshot>, Self::Error> {
        Ok(self.state.clone())
    }

    fn append(
        &mut self,
        expected_highest_sequence: u64,
        frame: &StoredFrame,
    ) -> Result<(), Self::Error> {
        let state = self.state.get_or_insert_with(OutboxSnapshot::default);
        debug_assert_eq!(
            state.highest_sequence, expected_highest_sequence,
            "the state machine validates the cursor before appending"
        );
        state.frames.push(frame.clone());
        state.highest_sequence = frame.sequence;
        Ok(())
    }

    fn record_acknowledgement(
        &mut self,
        _expected_ack_sequence: u64,
        ack_sequence: u64,
    ) -> Result<(), Self::Error> {
        if let Some(state) = self.state.as_mut() {
            state.ack_sequence = ack_sequence;
        }
        Ok(())
    }
}

impl CompactingOutbox for InMemoryOutbox {
    fn compact_through(&mut self, ack_sequence: u64) -> Result<usize, Self::Error> {
        let Some(state) = self.state.as_mut() else {
            return Ok(0);
        };
        let before = state.frames.len();
        state.frames.retain(|frame| frame.sequence > ack_sequence);
        Ok(before - state.frames.len())
    }
}

fn snapshot_of<S>(store: &mut S) -> Result<OutboxSnapshot, OutboxError<S::Error>>
where
    S: FrameOutbox,
{
    let snapshot = store
        .load()
        .map_err(OutboxError::Store)?
        .unwrap_or_default();
    snapshot.validate().map_err(OutboxError::CorruptState)?;
    Ok(snapshot)
}

fn validate_frame_shape(frame: &StoredFrame) -> Result<(), OutboxInputError> {
    if frame.sequence == 0 {
        return Err(OutboxInputError::ZeroSequence);
    }
    if frame.message_id.is_empty() {
        return Err(OutboxInputError::EmptyMessageId);
    }
    if frame.payload_digest.is_empty() {
        return Err(OutboxInputError::EmptyDigest);
    }
    if frame.frame.is_empty() {
        return Err(OutboxInputError::EmptyFrame);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(sequence: u64) -> StoredFrame {
        StoredFrame {
            message_id: format!("msg-{sequence}"),
            sequence,
            payload_digest: format!("sha256:{sequence:02x}"),
            frame: format!(r#"{{"sequence":{sequence}}}"#).into_bytes(),
        }
    }

    fn seeded(through: u64) -> (InMemoryOutbox, OutboxSession) {
        let mut store = InMemoryOutbox::new();
        let session = OutboxSession::new();
        for sequence in 1..=through {
            let expected = session.next_sequence(&mut store).expect("next sequence");
            session
                .enqueue(&mut store, expected, &stored(sequence))
                .expect("enqueue frame");
        }
        (store, session)
    }

    #[test]
    fn enqueue_persists_frames_and_advances_the_cursor() {
        let (mut store, session) = seeded(3);
        let snapshot = store.snapshot().expect("seeded snapshot");
        assert_eq!(snapshot.highest_sequence, 3);
        assert_eq!(snapshot.ack_sequence, 0);
        assert_eq!(session.next_sequence(&mut store), Ok(4));
    }

    #[test]
    fn enqueue_rejects_frames_off_the_stream_cursor() {
        let (mut store, session) = seeded(2);
        assert_eq!(
            session.enqueue(&mut store, 1, &stored(5)),
            Err(OutboxError::SequenceMismatch {
                expected: 1,
                found: 5
            }),
            "the frame must carry the caller's expected next sequence"
        );
        assert_eq!(
            session.enqueue(&mut store, 5, &stored(5)),
            Err(OutboxError::SequenceMismatch {
                expected: 3,
                found: 5
            }),
            "the frame must carry the stream's actual next sequence"
        );
        assert_eq!(
            session.enqueue(&mut store, 2, &stored(2)),
            Err(OutboxError::SequenceMismatch {
                expected: 3,
                found: 2
            }),
            "a stale expectation after another append is rejected"
        );
    }

    #[test]
    fn enqueue_rejects_invalid_frames() {
        let (mut store, session) = seeded(1);
        let mut bad = stored(2);
        bad.sequence = 0;
        assert_eq!(
            session.enqueue(&mut store, 1, &bad),
            Err(OutboxError::InvalidInput(OutboxInputError::ZeroSequence))
        );

        let mut bad = stored(2);
        bad.message_id = String::new();
        assert_eq!(
            session.enqueue(&mut store, 1, &bad),
            Err(OutboxError::InvalidInput(OutboxInputError::EmptyMessageId))
        );

        let mut bad = stored(2);
        bad.payload_digest = String::new();
        assert_eq!(
            session.enqueue(&mut store, 1, &bad),
            Err(OutboxError::InvalidInput(OutboxInputError::EmptyDigest))
        );

        let mut bad = stored(2);
        bad.frame = Vec::new();
        assert_eq!(
            session.enqueue(&mut store, 1, &bad),
            Err(OutboxError::InvalidInput(OutboxInputError::EmptyFrame))
        );

        let mut bad = stored(2);
        bad.message_id = "msg-1".to_owned();
        assert_eq!(
            session.enqueue(&mut store, 2, &bad),
            Err(OutboxError::InvalidInput(
                OutboxInputError::DuplicateMessageId("msg-1".to_owned())
            ))
        );
    }

    #[test]
    fn deliverable_pages_through_batches() {
        let (mut store, session) = seeded(5);
        let first = session.deliverable(&mut store, 0, 2).expect("first batch");
        assert_eq!(
            first
                .frames
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(first.ack_sequence, 0);
        assert_eq!(first.highest_sequence, 5);

        let second = session.deliverable(&mut store, 2, 2).expect("second batch");
        assert_eq!(
            second
                .frames
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );

        let rest = session.deliverable(&mut store, 4, 8).expect("last batch");
        assert_eq!(
            rest.frames
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![5]
        );
    }

    #[test]
    fn deliverable_rejects_bad_requests() {
        let (mut store, session) = seeded(2);
        assert_eq!(
            session.deliverable(&mut store, 0, 0),
            Err(OutboxError::InvalidInput(OutboxInputError::ZeroMaxFrames))
        );
        assert_eq!(
            session.deliverable(&mut store, 3, 1),
            Err(OutboxError::CursorAhead {
                requested: 3,
                highest: 2
            })
        );
    }

    #[test]
    fn acknowledge_only_accepts_monotonic_in_range_cursors() {
        let (mut store, session) = seeded(3);
        assert_eq!(session.acknowledge(&mut store, 2), Ok(2));
        assert_eq!(
            session.acknowledge(&mut store, 2),
            Ok(2),
            "repeating an ack is a no-op"
        );
        assert_eq!(
            session.acknowledge(&mut store, 1),
            Err(OutboxError::AckRegression {
                requested: 1,
                acknowledged: 2
            })
        );
        assert_eq!(
            session.acknowledge(&mut store, 4),
            Err(OutboxError::AckAhead {
                requested: 4,
                highest: 3
            })
        );
        assert_eq!(session.acknowledge(&mut store, 3), Ok(3));
        assert_eq!(store.snapshot().expect("snapshot").ack_sequence, 3);
    }

    #[test]
    fn compact_confirmed_keeps_the_cursors_and_unacked_frames() {
        let (mut store, session) = seeded(4);
        session.acknowledge(&mut store, 2).expect("acknowledge");
        assert_eq!(session.compact_confirmed(&mut store).expect("compact"), 2);

        let snapshot = store.snapshot().expect("snapshot");
        assert_eq!(
            snapshot.ack_sequence, 2,
            "the ack cursor survives compaction"
        );
        assert_eq!(
            snapshot.highest_sequence, 4,
            "the high-water mark survives compaction"
        );
        snapshot.validate().expect("compacted snapshot stays valid");

        let batch = session
            .deliverable(&mut store, 2, 8)
            .expect("resume delivery");
        assert_eq!(
            batch
                .frames
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![3, 4],
            "unconfirmed frames are never compacted away"
        );
    }

    #[test]
    fn recovery_from_a_persisted_snapshot_preserves_both_cursors() {
        let (store, _) = seeded(3);
        let mut recovered =
            InMemoryOutbox::from_snapshot(store.snapshot().cloned().expect("persisted snapshot"));
        let session = OutboxSession::new();
        assert_eq!(session.next_sequence(&mut recovered), Ok(4));
        assert_eq!(
            session
                .deliverable(&mut recovered, 0, 10)
                .expect("recovered batch")
                .frames
                .len(),
            3
        );
    }

    #[test]
    fn an_empty_stream_loads_as_none_and_defaults() {
        let mut store = InMemoryOutbox::new();
        let session = OutboxSession::new();
        assert!(store.load().expect("load empty").is_none());
        assert_eq!(session.next_sequence(&mut store), Ok(1));
    }

    #[test]
    fn snapshot_validation_rejects_corrupt_states() {
        let base = || {
            let mut snapshot = OutboxSnapshot::default();
            for sequence in 1..=3 {
                snapshot.frames.push(stored(sequence));
            }
            snapshot.highest_sequence = 3;
            snapshot
        };

        let mut snapshot = base();
        snapshot.ack_sequence = 4;
        assert_eq!(
            snapshot.validate(),
            Err(OutboxStateError::AckExceedsHighest {
                acknowledged: 4,
                highest: 3
            })
        );

        let mut snapshot = base();
        snapshot.ack_sequence = 3;
        snapshot.highest_sequence = 5;
        snapshot.frames = vec![stored(5)];
        assert_eq!(
            snapshot.validate(),
            Err(OutboxStateError::NonContiguous {
                expected: 4,
                found: 5
            }),
            "a retained range missing a confirmed frame is corrupt"
        );

        let mut snapshot = base();
        snapshot.frames[1].sequence = 5;
        assert!(matches!(
            snapshot.validate(),
            Err(OutboxStateError::NonContiguous { .. })
        ));

        let mut snapshot = base();
        snapshot.frames[0].sequence = 0;
        assert_eq!(snapshot.validate(), Err(OutboxStateError::ZeroSequence));

        let mut snapshot = base();
        snapshot.frames[2].message_id = snapshot.frames[1].message_id.clone();
        assert!(matches!(
            snapshot.validate(),
            Err(OutboxStateError::DuplicateMessageId(_))
        ));

        let mut snapshot = base();
        snapshot.frames[0].message_id = String::new();
        assert_eq!(snapshot.validate(), Err(OutboxStateError::EmptyMessageId));

        let mut snapshot = base();
        snapshot.frames[0].payload_digest = String::new();
        assert_eq!(snapshot.validate(), Err(OutboxStateError::EmptyDigest));

        let mut snapshot = base();
        snapshot.frames[0].frame = Vec::new();
        assert_eq!(snapshot.validate(), Err(OutboxStateError::EmptyFrame));

        let mut snapshot = base();
        snapshot.highest_sequence = 9;
        assert_eq!(
            snapshot.validate(),
            Err(OutboxStateError::HighestMismatch {
                stored: 9,
                derived: 3
            })
        );

        let mut snapshot = OutboxSnapshot::default();
        snapshot.frames.push(stored(u64::MAX));
        snapshot.highest_sequence = u64::MAX;
        assert_eq!(snapshot.validate(), Err(OutboxStateError::SequenceOverflow));
    }

    #[test]
    fn compaction_prefixes_are_valid_snapshots() {
        let (mut store, session) = seeded(3);
        session
            .acknowledge(&mut store, 3)
            .expect("acknowledge everything");
        session
            .compact_confirmed(&mut store)
            .expect("compact everything");
        let snapshot = store.snapshot().cloned().expect("snapshot");
        assert!(snapshot.frames.is_empty());
        assert_eq!(snapshot.ack_sequence, 3);
        assert_eq!(snapshot.highest_sequence, 3);
        snapshot
            .validate()
            .expect("a fully compacted stream stays valid");
        assert_eq!(
            session
                .deliverable(&mut store, 3, 4)
                .expect("empty batch")
                .frames,
            Vec::new()
        );
    }
}
