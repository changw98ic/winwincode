//! Durable, transport-neutral replay state for `ExecutionPort` streams.
//!
//! The state machine deliberately knows nothing about sockets, HTTP, generated
//! message DTOs, leases, or product projections.  Callers provide the stream
//! identity, a lease authority, and a durable store.  The store's `append`
//! operation is the atomic boundary for the accepted frame and its new
//! contiguous acknowledgement.

use std::collections::HashSet;

/// Sequence type used by one `ExecutionPort` worker stream.
pub type ReplaySequence = u64;

/// Stable key for one transport message stream.
///
/// The caller should derive this from the canonical execution identity (for
/// example job, lease, worker session, and stream kind).  It is intentionally
/// opaque here so this crate does not create a second product/event cursor.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReplayStreamKey(String);

impl ReplayStreamKey {
    /// Creates a stream key from the caller's canonical identity.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the canonical key value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Original event envelope retained for replay.
///
/// `frame` is the original transport-neutral encoded message.  It is returned
/// from duplicate and resume operations instead of rebuilding a message from
/// current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayFrame {
    pub event_id: String,
    pub sequence: ReplaySequence,
    pub digest: String,
    pub frame: Vec<u8>,
}

impl ReplayFrame {
    /// Constructs one frame.  Semantic validation happens at the state-machine
    /// boundary so a durable adapter can also validate records during recovery.
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

    /// Returns whether two frames represent the same event identity and
    /// semantic digest. The stored frame bytes remain the original envelope;
    /// callers may change transport-only fields such as a message id without
    /// changing the event's idempotency result.
    #[must_use]
    pub(crate) fn same_event_identity_and_digest(&self, other: &Self) -> bool {
        self.event_id == other.event_id
            && self.sequence == other.sequence
            && self.digest == other.digest
    }
}

/// Durable rows for one stream, loaded from the persistence adapter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplaySnapshot {
    /// Highest sequence that the Control Plane has confirmed as accepted.
    pub ack_sequence: ReplaySequence,
    /// Highest contiguous sequence in the stream retained by the Worker.
    /// This is a durable high-water mark; frames at or below `ack_sequence`
    /// may have been compacted.
    pub highest_sequence: ReplaySequence,
    /// Accepted original frames, ordered by sequence.
    pub events: Vec<ReplayFrame>,
}

impl ReplaySnapshot {
    /// Checks the invariants a durable adapter must preserve.
    ///
    /// # Errors
    ///
    /// Returns the precise invariant violation when stored events have invalid
    /// fields, are duplicated or non-contiguous, or disagree with the stored
    /// acknowledgement.
    /// Retained events may start after sequence one only when every omitted
    /// prefix frame is covered by `ack_sequence`.
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

/// Result of submitting one worker frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayDecision {
    /// The frame was durably appended and the retained sequence moved.
    Accepted { highest_sequence: ReplaySequence },
    /// The same event identity and semantic digest were seen again; the
    /// original frame is returned.
    Duplicate {
        highest_sequence: ReplaySequence,
        original: ReplayFrame,
    },
    /// A future sequence was received; no row or acknowledgement was written.
    Gap {
        highest_sequence: ReplaySequence,
        replay_from_sequence: ReplaySequence,
    },
    /// An event identity or sequence was reused with a different body.
    Conflict { highest_sequence: ReplaySequence },
}

/// Protocol-independent status carried by an acknowledgement for one replay
/// stream.  Generated wire enums are converted to this type at the boundary so
/// the replay state machine does not depend on a transport DTO.
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

/// Protocol-independent acknowledgement cursor and replay hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayAcknowledgement {
    /// The highest contiguous sequence confirmed by the Control Plane.
    pub ack_sequence: ReplaySequence,
    /// The outcome of the corresponding lease-scoped write.
    pub status: ReplayAcknowledgementStatus,
    /// The first missing sequence when `status` is [`Gap`](ReplayAcknowledgementStatus::Gap).
    pub replay_from_sequence: Option<ReplaySequence>,
}

/// Original frames requested after an acknowledged cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayBatch {
    /// Highest sequence acknowledged by the Control Plane.
    pub ack_sequence: ReplaySequence,
    /// Highest contiguous sequence retained by the Worker.
    pub highest_sequence: ReplaySequence,
    pub events: Vec<ReplayFrame>,
}

/// Caller-owned authority for the currently active lease and fencing token.
///
/// The replay state machine calls this before loading or writing stream state.
/// Lease expiry, worker-instance changes, and fencing-token comparisons stay
/// in the Control Plane's authority implementation.
pub trait ReplayAuthority {
    type Context;
    type Error;

    /// Confirms that the supplied stream still belongs to the active lease.
    ///
    /// # Errors
    ///
    /// Returns the authority implementation's error for an expired lease,
    /// stale fence, replaced Worker instance, or foreign stream identity.
    fn validate_active_lease(
        &self,
        stream: &ReplayStreamKey,
        context: &Self::Context,
    ) -> Result<(), Self::Error>;
}

/// Durable persistence seam for one replay stream.
///
/// `append` must atomically verify `expected_highest_sequence`, append `frame`,
/// and advance the retained highest sequence to
/// `expected_highest_sequence + 1`. A `SQLite` adapter can implement this with
/// one transaction; the state machine never treats its local snapshot as
/// durable truth.
pub trait ReplayStore {
    type Error;

    /// Loads the complete durable snapshot for one stream.
    ///
    /// # Errors
    ///
    /// Returns the persistence implementation's read error.
    fn load(&mut self, stream: &ReplayStreamKey) -> Result<Option<ReplaySnapshot>, Self::Error>;

    /// Atomically appends one frame if the durable retained-highest sequence
    /// still equals `expected_highest_sequence`.
    ///
    /// # Errors
    ///
    /// Returns the persistence implementation's transaction or race error.
    fn append(
        &mut self,
        stream: &ReplayStreamKey,
        expected_highest_sequence: ReplaySequence,
        frame: &ReplayFrame,
    ) -> Result<(), Self::Error>;
}

/// Durable acknowledgement seam for a Worker-owned replay stream.
///
/// The acknowledgement is the Control Plane's highest contiguous accepted
/// sequence. It is independent from the highest frame retained by the Worker.
/// Implementations must update the watermark atomically against
/// `expected_ack_sequence`; a Worker may then prune frames at or below that
/// watermark or retain them for a later compaction pass.
pub trait ReplayAcknowledgementStore: ReplayStore {
    /// Atomically advances the Control Plane acknowledgement watermark.
    ///
    /// # Errors
    ///
    /// Returns the adapter's transaction or acknowledgement-race error.
    fn record_acknowledgement(
        &mut self,
        stream: &ReplayStreamKey,
        expected_ack_sequence: ReplaySequence,
        ack_sequence: ReplaySequence,
    ) -> Result<(), Self::Error>;
}

/// Validation failures found in a durable replay snapshot.
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

/// Invalid caller input detected before any durable mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayInputError {
    EmptyStream,
    EmptyEventId,
    ZeroSequence,
    EmptyDigest,
    ZeroMaxEvents,
}

/// Failure from authority, persistence, or a corrupt durable snapshot.
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

/// Stateless replay state machine backed by a caller-provided durable store.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReplayStateMachine;

impl ReplayStateMachine {
    /// Creates a state machine.  All accepted state remains in the store.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Validates authority, then accepts one event or returns a stable replay
    /// decision.  Only the accepted branch calls `ReplayStore::append`.
    ///
    /// # Errors
    ///
    /// Returns an authority or store error, invalid caller input, or a precise
    /// durable-state corruption error. No write occurs for rejected branches.
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

        let snapshot = load_snapshot(store, stream)?;
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

    /// Loads original frames strictly after `after_sequence`, without writing.
    ///
    /// # Errors
    ///
    /// Returns an authority or store error, a zero page size, a cursor beyond
    /// the retained high-water mark, or a precise durable-state corruption error.
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

        let snapshot = load_snapshot(store, stream)?;
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

    /// Advances the Control Plane acknowledgement watermark without changing
    /// the Worker-retained frames.
    ///
    /// # Errors
    ///
    /// Returns an authority or store error, a watermark ahead of the retained
    /// range, a regressing watermark, or a corrupt durable snapshot.
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
        let snapshot = load_snapshot(store, stream)?;
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
