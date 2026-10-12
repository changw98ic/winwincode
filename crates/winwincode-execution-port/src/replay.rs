use std::collections::HashSet;

pub type ReplaySequence = u64;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReplayStreamKey(String);

impl ReplayStreamKey {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayFrame {
    pub event_id: String,
    pub sequence: ReplaySequence,
    pub digest: String,
    pub frame: Vec<u8>,
}

impl ReplayFrame {
    #[must_use]
    pub fn new(
        event_id: impl Into<String>,
        sequence: ReplaySequence,
        digest: impl Into<String>,
        frame: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            sequence,
            digest: digest.into(),
            frame: frame.into(),
        }
    }

    #[must_use]
    pub(crate) fn same_event_identity_and_digest(&self, other: &Self) -> bool {
        self.event_id == other.event_id
            && self.sequence == other.sequence
            && self.digest == other.digest
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplaySnapshot {
    pub ack_sequence: ReplaySequence,
    pub highest_sequence: ReplaySequence,
    pub events: Vec<ReplayFrame>,
}

impl ReplaySnapshot {
    pub fn validate(&self) -> Result<(), ReplayStateError> {
        if self.ack_sequence > self.highest_sequence {
            return Err(ReplayStateError::AckExceedsHighest {
                acknowledged: self.ack_sequence,
                highest: self.highest_sequence,
            });
        }

        let first_sequence = self.events.first().map_or(1, |event| event.sequence);
        let mut expected = first_sequence;
        let mut event_ids = HashSet::with_capacity(self.events.len());

        for event in &self.events {
            if event.sequence == 0 {
                return Err(ReplayStateError::ZeroSequence);
            }
            if event.event_id.is_empty() {
                return Err(ReplayStateError::EmptyEventId);
            }
            if event.digest.is_empty() {
                return Err(ReplayStateError::EmptyDigest);
            }
            if !event_ids.insert(event.event_id.clone()) {
                return Err(ReplayStateError::DuplicateEventId(event.event_id.clone()));
            }
            if event.sequence != expected {
                return Err(ReplayStateError::NonContiguous {
                    expected,
                    found: event.sequence,
                });
            }
            expected = expected
                .checked_add(1)
                .ok_or(ReplayStateError::SequenceOverflow)?;
        }

        if self
            .ack_sequence
            .checked_add(1)
            .is_some_and(|first_unacknowledged| first_sequence > first_unacknowledged)
        {
            return Err(ReplayStateError::NonContiguous {
                expected: self.ack_sequence.saturating_add(1),
                found: first_sequence,
            });
        }

        let derived_highest = self
            .events
            .last()
            .map_or(self.ack_sequence, |event| event.sequence);
        if self.highest_sequence != derived_highest {
            return Err(ReplayStateError::HighestMismatch {
                stored: self.highest_sequence,
                derived: derived_highest,
            });
        }
        Ok(())
    }

    fn event_by_id(&self, event_id: &str) -> Option<&ReplayFrame> {
        self.events.iter().find(|event| event.event_id == event_id)
    }

    fn event_by_sequence(&self, sequence: ReplaySequence) -> Option<&ReplayFrame> {
        self.events.iter().find(|event| event.sequence == sequence)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayDecision {
    Accepted {
        highest_sequence: ReplaySequence,
    },
    Duplicate {
        highest_sequence: ReplaySequence,
        original: ReplayFrame,
    },
    Gap {
        highest_sequence: ReplaySequence,
        replay_from_sequence: ReplaySequence,
    },
    Conflict {
        highest_sequence: ReplaySequence,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayAcknowledgementStatus {
    Accepted,
    Duplicate,
    Gap,
    RejectedConflict,
    RejectedExpiredLease,
    RejectedStaleFencingToken,
    RejectedWorkerInstance,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayAcknowledgement {
    pub ack_sequence: ReplaySequence,
    pub status: ReplayAcknowledgementStatus,
    pub replay_from_sequence: Option<ReplaySequence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayBatch {
    pub ack_sequence: ReplaySequence,
    pub highest_sequence: ReplaySequence,
    pub events: Vec<ReplayFrame>,
}

pub trait ReplayAuthority {
    type Context;
    type Error;

    fn validate_active_lease(
        &self,
        stream: &ReplayStreamKey,
        context: &Self::Context,
    ) -> Result<(), Self::Error>;
}

pub trait ReplayStore {
    type Error;

    fn load(&mut self, stream: &ReplayStreamKey) -> Result<Option<ReplaySnapshot>, Self::Error>;

    fn load_indexed(
        &mut self,
        _stream: &ReplayStreamKey,
        _query: ReplayReadQuery<'_>,
    ) -> Result<Option<ReplaySnapshot>, Self::Error> {
        Ok(None)
    }

    fn load_watermarks(&mut self, stream: &ReplayStreamKey) -> Result<ReplaySnapshot, Self::Error> {
        if let Some(snapshot) = self.load_indexed(stream, ReplayReadQuery::Watermarks)? {
            return Ok(snapshot);
        }
        Ok(self.load(stream)?.unwrap_or_default())
    }

    fn append(
        &mut self,
        stream: &ReplayStreamKey,
        expected_highest_sequence: ReplaySequence,
        frame: &ReplayFrame,
    ) -> Result<(), Self::Error>;
}

#[derive(Clone, Copy, Debug)]
pub enum ReplayReadQuery<'a> {
    Watermarks,
    Conflict {
        event_id: &'a str,
        sequence: ReplaySequence,
    },
    After {
        sequence: ReplaySequence,
        max_events: usize,
    },
}

pub trait ReplayAcknowledgementStore: ReplayStore {
    fn record_acknowledgement(
        &mut self,
        stream: &ReplayStreamKey,
        expected_ack_sequence: ReplaySequence,
        ack_sequence: ReplaySequence,
    ) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayStateError {
    ZeroSequence,
    EmptyEventId,
    EmptyDigest,
    NonContiguous {
        expected: ReplaySequence,
        found: ReplaySequence,
    },
    DuplicateEventId(String),
    HighestMismatch {
        stored: ReplaySequence,
        derived: ReplaySequence,
    },
    AckExceedsHighest {
        acknowledged: ReplaySequence,
        highest: ReplaySequence,
    },
    SequenceOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayInputError {
    EmptyStream,
    EmptyEventId,
    ZeroSequence,
    EmptyDigest,
    ZeroMaxEvents,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplayError<AuthorityError, StoreError> {
    Authority(AuthorityError),
    Store(StoreError),
    InvalidInput(ReplayInputError),
    CorruptState(ReplayStateError),
    CursorAhead {
        requested: ReplaySequence,
        highest: ReplaySequence,
    },
    AckAhead {
        requested: ReplaySequence,
        highest: ReplaySequence,
    },
    AckRegression {
        requested: ReplaySequence,
        acknowledged: ReplaySequence,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReplayStateMachine;

impl ReplayStateMachine {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    pub fn accept<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        stream: &ReplayStreamKey,
        context: &A::Context,
        frame: &ReplayFrame,
    ) -> Result<ReplayDecision, ReplayError<A::Error, S::Error>>
    where
        S: ReplayStore,
        A: ReplayAuthority,
    {
        authority
            .validate_active_lease(stream, context)
            .map_err(ReplayError::Authority)?;
        validate_frame(stream, frame).map_err(ReplayError::InvalidInput)?;

        let snapshot = load_selected(
            store,
            stream,
            ReplayReadQuery::Conflict {
                event_id: &frame.event_id,
                sequence: frame.sequence,
            },
        )?;
        let expected = snapshot
            .highest_sequence
            .checked_add(1)
            .ok_or(ReplayStateError::SequenceOverflow)
            .map_err(ReplayError::CorruptState)?;

        if let Some(original) = snapshot.event_by_id(&frame.event_id) {
            if original.same_event_identity_and_digest(frame) {
                return Ok(ReplayDecision::Duplicate {
                    highest_sequence: snapshot.highest_sequence,
                    original: original.clone(),
                });
            }
            return Ok(ReplayDecision::Conflict {
                highest_sequence: snapshot.highest_sequence,
            });
        }

        if snapshot.event_by_sequence(frame.sequence).is_some() {
            return Ok(ReplayDecision::Conflict {
                highest_sequence: snapshot.highest_sequence,
            });
        }

        if frame.sequence > expected {
            return Ok(ReplayDecision::Gap {
                highest_sequence: snapshot.highest_sequence,
                replay_from_sequence: expected,
            });
        }

        if frame.sequence < expected {
            return Ok(ReplayDecision::Conflict {
                highest_sequence: snapshot.highest_sequence,
            });
        }

        store
            .append(stream, snapshot.highest_sequence, frame)
            .map_err(ReplayError::Store)?;
        Ok(ReplayDecision::Accepted {
            highest_sequence: frame.sequence,
        })
    }

    pub fn resume<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        stream: &ReplayStreamKey,
        context: &A::Context,
        after_sequence: ReplaySequence,
        max_events: usize,
    ) -> Result<ReplayBatch, ReplayError<A::Error, S::Error>>
    where
        S: ReplayStore,
        A: ReplayAuthority,
    {
        authority
            .validate_active_lease(stream, context)
            .map_err(ReplayError::Authority)?;
        if max_events == 0 {
            return Err(ReplayError::InvalidInput(ReplayInputError::ZeroMaxEvents));
        }

        let snapshot = load_selected(
            store,
            stream,
            ReplayReadQuery::After {
                sequence: after_sequence,
                max_events,
            },
        )?;
        if after_sequence > snapshot.highest_sequence {
            return Err(ReplayError::CursorAhead {
                requested: after_sequence,
                highest: snapshot.highest_sequence,
            });
        }

        let events = snapshot
            .events
            .into_iter()
            .filter(|event| event.sequence > after_sequence)
            .take(max_events)
            .collect();
        Ok(ReplayBatch {
            ack_sequence: snapshot.ack_sequence,
            highest_sequence: snapshot.highest_sequence,
            events,
        })
    }

    pub fn acknowledge<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        stream: &ReplayStreamKey,
        context: &A::Context,
        ack_sequence: ReplaySequence,
    ) -> Result<ReplaySequence, ReplayError<A::Error, S::Error>>
    where
        S: ReplayAcknowledgementStore,
        A: ReplayAuthority,
    {
        authority
            .validate_active_lease(stream, context)
            .map_err(ReplayError::Authority)?;
        if stream.as_str().is_empty() {
            return Err(ReplayError::InvalidInput(ReplayInputError::EmptyStream));
        }
        let snapshot = load_selected(store, stream, ReplayReadQuery::Watermarks)?;
        if ack_sequence > snapshot.highest_sequence {
            return Err(ReplayError::AckAhead {
                requested: ack_sequence,
                highest: snapshot.highest_sequence,
            });
        }
        if ack_sequence < snapshot.ack_sequence {
            return Err(ReplayError::AckRegression {
                requested: ack_sequence,
                acknowledged: snapshot.ack_sequence,
            });
        }
        if ack_sequence == snapshot.ack_sequence {
            return Ok(ack_sequence);
        }
        store
            .record_acknowledgement(stream, snapshot.ack_sequence, ack_sequence)
            .map_err(ReplayError::Store)?;
        Ok(ack_sequence)
    }
}

fn validate_frame(stream: &ReplayStreamKey, frame: &ReplayFrame) -> Result<(), ReplayInputError> {
    if stream.as_str().is_empty() {
        return Err(ReplayInputError::EmptyStream);
    }
    if frame.event_id.is_empty() {
        return Err(ReplayInputError::EmptyEventId);
    }
    if frame.sequence == 0 {
        return Err(ReplayInputError::ZeroSequence);
    }
    if frame.digest.is_empty() {
        return Err(ReplayInputError::EmptyDigest);
    }
    Ok(())
}

fn load_snapshot<AuthorityError, S>(
    store: &mut S,
    stream: &ReplayStreamKey,
) -> Result<ReplaySnapshot, ReplayError<AuthorityError, S::Error>>
where
    S: ReplayStore,
{
    let snapshot = store
        .load(stream)
        .map_err(ReplayError::Store)?
        .unwrap_or_default();
    snapshot.validate().map_err(ReplayError::CorruptState)?;
    Ok(snapshot)
}

fn load_selected<AuthorityError, S>(
    store: &mut S,
    stream: &ReplayStreamKey,
    query: ReplayReadQuery<'_>,
) -> Result<ReplaySnapshot, ReplayError<AuthorityError, S::Error>>
where
    S: ReplayStore,
{
    let Some(snapshot) = store
        .load_indexed(stream, query)
        .map_err(ReplayError::Store)?
    else {
        return load_snapshot(store, stream);
    };
    if snapshot.ack_sequence > snapshot.highest_sequence {
        return Err(ReplayError::CorruptState(
            ReplayStateError::AckExceedsHighest {
                acknowledged: snapshot.ack_sequence,
                highest: snapshot.highest_sequence,
            },
        ));
    }
    Ok(snapshot)
}
