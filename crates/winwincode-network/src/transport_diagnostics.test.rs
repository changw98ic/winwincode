// SPDX-License-Identifier: Apache-2.0

use super::{Duration, ExchangeIo, NextTimeout};
use ureq::{Error, Timeout, unversioned::transport::time::Duration as TransportDuration};

#[test]
fn expired_deadline_stops_before_the_socket_timeout_fallback() {
    let io = ExchangeIo::new(Duration::from_secs(1), Duration::from_millis(250));
    for body_started in [false, true] {
        if body_started {
            io.body_started();
        }
        let expired = NextTimeout {
            after: Duration::ZERO.into(),
            reason: Timeout::Global,
        };
        assert_eq!(expired.not_zero(), Some(Duration::from_secs(1).into()));
        assert!(matches!(
            io.timeout(expired),
            Err(Error::Timeout(Timeout::Global))
        ));
    }
}

#[test]
fn live_deadline_uses_the_smaller_remaining_or_idle_duration() {
    let io = ExchangeIo::new(Duration::from_secs(1), Duration::from_millis(250));
    io.body_started();
    for (remaining, expected) in [
        (Duration::from_millis(10), Duration::from_millis(10)),
        (Duration::from_millis(500), Duration::from_millis(250)),
    ] {
        let next = NextTimeout {
            after: remaining.into(),
            reason: Timeout::Global,
        };
        assert_eq!(io.timeout(next).unwrap(), expected);
    }
}

#[test]
fn no_global_deadline_keeps_the_progress_idle_timeout() {
    let io = ExchangeIo::new(Duration::from_secs(1), Duration::from_millis(250));
    io.body_started();
    assert_eq!(
        io.timeout(NextTimeout {
            after: TransportDuration::NotHappening,
            reason: Timeout::Global,
        })
        .unwrap(),
        Duration::from_millis(250)
    );
}

#[test]
fn actual_blocking_socket_deadlines_preserve_timeout_cause_after_opening_and_body_start() {
    use ureq::unversioned::transport::Transport as _;

    for body_started in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let io = ExchangeIo::new(Duration::from_millis(10), Duration::from_millis(30));
        if body_started {
            io.body_started();
        }
        let mut transport = super::ExchangeTransport {
            socket,
            buffers: ureq::unversioned::transport::LazyBuffers::new(1024, 1024),
            io,
        };
        let error = transport
            .await_input(NextTimeout {
                after: TransportDuration::NotHappening,
                reason: Timeout::Global,
            })
            .unwrap_err();
        let failure = crate::classify_ureq(&error, false, crate::Phase::ResponseHeaders);
        assert_eq!(failure.kind, crate::ErrorKind::Timeout);
        assert_eq!(failure.acceptance, crate::Acceptance::Unknown);
        assert_eq!(failure.phase, crate::Phase::ResponseHeaders);
    }
}
