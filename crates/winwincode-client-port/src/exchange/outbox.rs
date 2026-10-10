use std::collections::HashSet;

use crate::exchange::frame::StoredFrame;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxSnapshot {
    pub ack_sequence: u64,
    pub highest_sequence: u64,
    pub frames: Vec<StoredFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxStateError {
    ZeroSequence,
    EmptyMessageId,
    EmptyDigest,
    EmptyFrame,
    DuplicateMessageId(String),
    NonContiguous { expected: u64, found: u64 },
    HighestMismatch { stored: u64, derived: u64 },
    AckExceedsHighest { acknowledged: u64, highest: u64 },
    SequenceOverflow,
}

impl OutboxSnapshot {
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

pub trait FrameOutbox {
    type Error;

    fn load(&mut self) -> Result<Option<OutboxSnapshot>, Self::Error>;

    fn append(
        &mut self,
        expected_highest_sequence: u64,
        frame: &StoredFrame,
    ) -> Result<(), Self::Error>;

    fn record_acknowledgement(
        &mut self,
        expected_ack_sequence: u64,
        ack_sequence: u64,
    ) -> Result<(), Self::Error>;
}

pub trait CompactingOutbox: FrameOutbox {
    fn compact_through(&mut self, ack_sequence: u64) -> Result<usize, Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxInputError {
    EmptyMessageId,
    ZeroSequence,
    EmptyDigest,
    EmptyFrame,
    DuplicateMessageId(String),
    ZeroMaxFrames,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OutboxError<E> {
    Store(E),
    InvalidInput(OutboxInputError),
    CorruptState(OutboxStateError),
    SequenceMismatch { expected: u64, found: u64 },
    SequenceExhausted,
    CursorAhead { requested: u64, highest: u64 },
    AckAhead { requested: u64, highest: u64 },
    AckRegression { requested: u64, acknowledged: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxBatch {
    pub ack_sequence: u64,
    pub highest_sequence: u64,
    pub frames: Vec<StoredFrame>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OutboxSession;

impl OutboxSession {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InMemoryOutbox {
    state: Option<OutboxSnapshot>,
}

impl InMemoryOutbox {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_snapshot(snapshot: OutboxSnapshot) -> Self {
        Self {
            state: Some(snapshot),
        }
    }

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
