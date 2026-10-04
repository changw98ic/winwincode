// SPDX-License-Identifier: Apache-2.0

//! A paired wall/monotonic anchor. A retained anchor is never the current time.

use winwincode_domain::Instant;

pub(super) struct DriverClock {
    wall_anchor: Instant,
    monotonic_anchor: std::time::Instant,
}

impl DriverClock {
    pub(super) fn new(wall_anchor: Instant, monotonic_anchor: std::time::Instant) -> Self {
        Self {
            wall_anchor,
            monotonic_anchor,
        }
    }

    pub(super) fn observe(&mut self, wall: Instant, observed_at: std::time::Instant) {
        let (Some(current), Some(estimated)) = (parse(&wall), self.effective_at(observed_at))
        else {
            return;
        };
        // A delayed sample or clock rollback must not discard elapsed time.
        if current >= estimated {
            self.wall_anchor = wall;
            self.monotonic_anchor = observed_at;
        }
    }

    pub(super) fn anchor(&self) -> (&Instant, std::time::Instant) {
        (&self.wall_anchor, self.monotonic_anchor)
    }

    fn effective_at(&self, observed_at: std::time::Instant) -> Option<time::OffsetDateTime> {
        let elapsed =
            time::Duration::try_from(observed_at.checked_duration_since(self.monotonic_anchor)?)
                .ok()?;
        parse(&self.wall_anchor)?.checked_add(elapsed)
    }

    pub(super) fn timestamp(&self) -> Option<Instant> {
        let format = time::format_description::parse(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
        )
        .ok()?;
        Some(Instant(
            self.effective_at(std::time::Instant::now())?
                .format(&format)
                .ok()?,
        ))
    }

    pub(super) fn start_deadline(
        &self,
        issued: &Instant,
        expires: &Instant,
    ) -> Option<std::time::Instant> {
        let now = self.effective_at(std::time::Instant::now())?;
        let expires = parse(expires)?;
        if now < parse(issued)? || now >= expires {
            return None;
        }
        // Keep the original paired origin; rounding a projected wall timestamp
        // and attaching a fresh origin could extend the expiry again.
        self.monotonic_anchor
            .checked_add(std::time::Duration::try_from(expires - parse(&self.wall_anchor)?).ok()?)
    }
}

fn parse(value: &Instant) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(&value.0, &time::format_description::well_known::Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    use super::{DriverClock, Instant, parse};

    #[test]
    fn observations_preserve_elapsed_time_and_rejection_timestamps() {
        let origin = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(10_005))
            .unwrap();
        let anchor = Instant("2030-01-01T07:00:00.000Z".into());
        let mut clock = DriverClock::new(anchor.clone(), origin);
        let sampled_at = origin + std::time::Duration::from_millis(10_005);
        clock.observe(Instant("2030-01-01T07:00:10.000Z".into()), sampled_at);
        assert_eq!(clock.anchor(), (&anchor, origin));
        assert_eq!(
            clock.effective_at(sampled_at).unwrap(),
            parse(&Instant("2030-01-01T07:00:10.005Z".into())).unwrap()
        );
        assert!(clock.timestamp().unwrap().0.as_str() >= "2030-01-01T07:00:10.005Z");
        clock.observe(Instant("2030-01-01T06:59:00.000Z".into()), sampled_at);
        assert_eq!(clock.anchor(), (&anchor, origin));
        let forward = Instant("2030-01-01T07:00:20.000Z".into());
        clock.observe(forward.clone(), sampled_at);
        assert_eq!(clock.anchor(), (&forward, sampled_at));
    }
}
