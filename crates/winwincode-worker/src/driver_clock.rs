// SPDX-License-Identifier: Apache-2.0

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
        let timestamp = parse(&self.wall_anchor)?.checked_add(elapsed)?;
        (0..=9999).contains(&timestamp.year()).then_some(timestamp)
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
        self.monotonic_anchor
            .checked_add(std::time::Duration::try_from(expires - parse(&self.wall_anchor)?).ok()?)
    }
}

fn parse(value: &Instant) -> Option<time::OffsetDateTime> {
    if !winwincode_execution_port::execution_identity::canonical_instant(value) {
        return None;
    }
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

    #[test]
    fn driver_and_checkpoint_deadlines_reject_noncanonical_authority_time() {
        use crate::change_batch_store::CheckpointAcceptanceDeadline;
        use winwincode_execution_port::generated::ExecutionLeaseStamp;
        let message: winwincode_execution_port::generated::ExecutionPortMessage =
            serde_json::from_value(
                serde_json::from_str::<serde_json::Value>(include_str!(
                    "../../../tests/fixtures/contracts/execution-port.valid.json"
                ))
                .unwrap()["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["kind"] == "model.open")
                    .unwrap()
                    .clone(),
            )
            .unwrap();
        let winwincode_execution_port::generated::ExecutionPortMessage::ModelOpenMessage(open) =
            message
        else {
            panic!("model open fixture");
        };
        let lease = ExecutionLeaseStamp {
            issued_at: Instant("2030-01-01T00:00:00.000Z".into()),
            expires_at: Instant("2030-01-01T00:01:00.000Z".into()),
            ..open.lease
        };
        let now = Instant("2030-01-01T00:00:10.000Z".into());
        assert!(
            CheckpointAcceptanceDeadline::from_lease_observation(
                &lease,
                &now,
                std::time::Instant::now()
            )
            .is_some()
        );
        for value in [
            "2030-01-01T00:00:10Z",
            "2030-01-01T00:00:10.000+00:00",
            "2030-01-01T00:00:10.000+08:00",
            "2030-02-30T00:00:10.000Z",
            "2030-01-01T00:00:60.000Z",
        ] {
            let invalid = Instant(value.to_owned());
            assert!(
                DriverClock::new(invalid.clone(), std::time::Instant::now())
                    .timestamp()
                    .is_none()
            );
            assert!(
                CheckpointAcceptanceDeadline::from_lease_observation(
                    &lease,
                    &invalid,
                    std::time::Instant::now()
                )
                .is_none()
            );
            let invalid_issued = ExecutionLeaseStamp {
                issued_at: invalid.clone(),
                ..lease.clone()
            };
            assert!(
                CheckpointAcceptanceDeadline::from_lease_observation(
                    &invalid_issued,
                    &now,
                    std::time::Instant::now()
                )
                .is_none()
            );
            let invalid_expiry = ExecutionLeaseStamp {
                expires_at: invalid,
                ..lease.clone()
            };
            assert!(
                CheckpointAcceptanceDeadline::from_lease_observation(
                    &invalid_expiry,
                    &now,
                    std::time::Instant::now()
                )
                .is_none()
            );
        }
    }

    #[test]
    fn elapsed_time_outside_the_canonical_year_range_fails_closed() {
        let origin = std::time::Instant::now();
        let clock = DriverClock::new(Instant("9999-12-31T23:59:59.999Z".into()), origin);
        assert!(
            clock
                .effective_at(origin + std::time::Duration::from_millis(1))
                .is_none()
        );
    }
}
