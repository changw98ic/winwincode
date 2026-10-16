// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, Mutex};

use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use winwincode_domain::Instant;

#[derive(Clone, Default)]
pub(crate) struct AuthorityClock {
    anchor: Arc<Mutex<Option<AuthorityAnchor>>>,
}

struct AuthorityAnchor {
    timestamp: OffsetDateTime,
    observed_at: std::time::Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthorityClockError {
    Unavailable,
    InvalidTime,
}

impl AuthorityClock {
    #[cfg(test)]
    pub(crate) fn advance_for_test(&self, elapsed: std::time::Duration) {
        self.anchor.lock().unwrap().as_mut().unwrap().observed_at -= elapsed;
    }

    pub(crate) fn observe(&self, now: &Instant) -> Result<(), AuthorityClockError> {
        let observed = parse_timestamp(now).ok_or(AuthorityClockError::InvalidTime)?;
        let mut anchor = self
            .anchor
            .lock()
            .map_err(|_| AuthorityClockError::Unavailable)?;
        let observed_at = std::time::Instant::now();
        let timestamp = if let Some(current) = anchor.as_ref() {
            effective_timestamp(current, observed_at)?.max(observed)
        } else {
            observed
        };
        *anchor = Some(AuthorityAnchor {
            timestamp,
            observed_at,
        });
        Ok(())
    }

    pub(crate) fn now(&self) -> Result<Option<Instant>, AuthorityClockError> {
        let anchor = self
            .anchor
            .lock()
            .map_err(|_| AuthorityClockError::Unavailable)?;
        anchor
            .as_ref()
            .map(|anchor| {
                effective_timestamp(anchor, std::time::Instant::now()).map(format_timestamp)
            })
            .transpose()
    }
}

pub(crate) fn canonical_instant(instant: &Instant) -> bool {
    parse_timestamp(instant).is_some()
}

fn parse_timestamp(instant: &Instant) -> Option<OffsetDateTime> {
    if !winwincode_execution_port::execution_identity::canonical_instant(instant) {
        return None;
    }
    let timestamp = OffsetDateTime::parse(&instant.0, &Rfc3339).ok()?;
    (format_timestamp(timestamp) == *instant).then_some(timestamp)
}

fn effective_timestamp(
    anchor: &AuthorityAnchor,
    now: std::time::Instant,
) -> Result<OffsetDateTime, AuthorityClockError> {
    let elapsed = now
        .checked_duration_since(anchor.observed_at)
        .ok_or(AuthorityClockError::InvalidTime)?;
    let elapsed =
        time::Duration::try_from(elapsed).map_err(|_| AuthorityClockError::InvalidTime)?;
    let timestamp = anchor
        .timestamp
        .checked_add(elapsed)
        .ok_or(AuthorityClockError::InvalidTime)?;
    if !(0..=9999).contains(&timestamp.year()) {
        return Err(AuthorityClockError::InvalidTime);
    }
    Ok(timestamp)
}

fn format_timestamp(timestamp: OffsetDateTime) -> Instant {
    Instant(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        timestamp.year(),
        u8::from(timestamp.month()),
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second(),
        timestamp.millisecond(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{AuthorityClock, canonical_instant};
    use winwincode_domain::Instant;

    #[test]
    fn authority_clock_advances_without_a_new_owner_observation() {
        let clock = AuthorityClock::default();
        let initial = Instant("2030-01-01T00:00:00.000Z".into());
        clock.observe(&initial).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(clock.now().unwrap().unwrap().0.as_str() >= "2030-01-01T00:00:00.030Z");
        clock.observe(&initial).unwrap();
        assert!(clock.now().unwrap().unwrap().0.as_str() >= "2030-01-01T00:00:00.030Z");
    }

    #[test]
    fn authority_clock_rejects_noncanonical_and_invalid_calendar_timestamps() {
        for value in [
            "2030-01-01T00:00:00Z",
            "2030-01-01T00:00:00.000+00:00",
            "2030-02-30T00:00:00.000Z",
            "2030-01-01T00:00:60.000Z",
        ] {
            let timestamp = Instant(value.into());
            assert!(!canonical_instant(&timestamp));
            assert!(AuthorityClock::default().observe(&timestamp).is_err());
        }
    }
}
