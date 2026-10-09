// SPDX-License-Identifier: Apache-2.0

//! The embedded MCP SDK delegates retry decisions to the same product policy.
use crate::{Acceptance, ErrorKind, NetworkFailure, Phase, Replay};
use codex_exec_server::HttpNetworkErrorKind;

pub fn install_retry_policy() {
    codex_rmcp_client::set_network_retry_policy(std::sync::Arc::new(
        |attempt, connections, facts| {
            let failure = if let Some(status) = facts.status {
                NetworkFailure::http(
                    status,
                    facts
                        .retry_after
                        .as_deref()
                        .and_then(|value| crate::retry_after(value, std::time::SystemTime::now())),
                )
            } else if let Some(transport) = facts.transport {
                NetworkFailure::new(
                    match transport.kind {
                        HttpNetworkErrorKind::ConnectionUnavailable => {
                            ErrorKind::ConnectionUnavailable
                        }
                        HttpNetworkErrorKind::TransportInterrupted => {
                            ErrorKind::TransportInterrupted
                        }
                        HttpNetworkErrorKind::Timeout => ErrorKind::Timeout,
                        HttpNetworkErrorKind::TlsInvalid => ErrorKind::TlsInvalid,
                        HttpNetworkErrorKind::RequestInvalid => ErrorKind::RequestInvalid,
                        HttpNetworkErrorKind::Authorization => ErrorKind::Authorization,
                    },
                    if transport.not_sent {
                        Acceptance::NotSent
                    } else {
                        Acceptance::Unknown
                    },
                    Phase::ResponseHeaders,
                )
            } else {
                NetworkFailure::new(
                    ErrorKind::TransportInterrupted,
                    Acceptance::Unknown,
                    Phase::Stream,
                )
            };
            crate::decide(
                failure,
                if facts.reconcile_first {
                    Replay::ReconcileFirst
                } else {
                    Replay::ReplayExact
                },
                attempt,
                connections,
                crate::defaults().max_attempts,
                0,
            )
            .immediate_delay()
        },
    ));
}
